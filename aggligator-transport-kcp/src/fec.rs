//! Forward Error Correction (FEC) transport wrapper for KCP.
//!
//! Wraps any [`Transport`] implementation with Reed-Solomon erasure coding at
//! the UDP datagram level. This allows lost packets to be reconstructed from
//! parity shards without waiting for KCP retransmission, significantly
//! reducing latency on lossy networks.
//!
//! # Protocol
//!
//! Outgoing datagrams are grouped into blocks of `data_shards` packets.
//! For each block, `parity_shards` parity packets are computed and appended.
//! Every packet carries a small header:
//!
//! ```text
//! [block_id: u32][shard_index: u8][shard_count: u8][parity_count: u8][payload_len: u16][payload...]
//! ```
//!
//! On the receive side, blocks are reassembled. Once enough shards arrive
//! (at least `data_shards` out of `data_shards + parity_shards`), any
//! missing data shards are reconstructed and the original datagrams are
//! delivered in order.

use kcp_tokio::Transport;
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// How long to wait before flushing an incomplete FEC block.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(50);

/// FEC header size in bytes.
///
/// `[block_id: 4][shard_idx: 1][shard_count: 1][parity_count: 1][payload_len: 2]`
const FEC_HEADER_SIZE: usize = 9;

/// Maximum shard payload size (MTU minus FEC header, with margin).
const MAX_SHARD_PAYLOAD: usize = 1500;

/// Configuration for FEC encoding.
#[derive(Debug, Clone)]
pub struct FecConfig {
    /// Number of data shards per FEC block.
    ///
    /// More data shards = larger blocks = higher throughput but higher latency
    /// before parity can be sent. Default: 3.
    pub data_shards: usize,
    /// Number of parity shards per FEC block.
    ///
    /// More parity shards = more overhead but can recover more lost packets.
    /// Default: 1 (can tolerate 1 lost packet per block).
    pub parity_shards: usize,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self { data_shards: 3, parity_shards: 1 }
    }
}

impl FecConfig {
    /// Create a new FEC configuration.
    ///
    /// # Panics
    ///
    /// Panics if `data_shards` is 0, `parity_shards` is 0, or their sum exceeds 255.
    pub fn new(data_shards: usize, parity_shards: usize) -> Self {
        assert!(data_shards > 0, "data_shards must be > 0");
        assert!(parity_shards > 0, "parity_shards must be > 0");
        assert!(
            data_shards + parity_shards <= 255,
            "total shards (data + parity) must fit in u8, i.e. <= 255"
        );
        Self { data_shards, parity_shards }
    }

    /// Bandwidth overhead ratio (parity / data).
    pub fn overhead(&self) -> f64 {
        self.parity_shards as f64 / self.data_shards as f64
    }
}

/// Encoder state for outgoing packets.
struct FecEncoder {
    rs: ReedSolomon,
    data_shards: usize,
    parity_shards: usize,
    block_id: u32,
    /// Buffered data shard payloads for the current block.
    shard_buf: Vec<Vec<u8>>,
    /// Original payload lengths (before padding).
    shard_lens: Vec<usize>,
}

impl FecEncoder {
    fn new(data_shards: usize, parity_shards: usize) -> Self {
        let rs = ReedSolomon::new(data_shards, parity_shards)
            .expect("invalid FEC shard configuration");
        Self { rs, data_shards, parity_shards, block_id: 0, shard_buf: Vec::new(), shard_lens: Vec::new() }
    }

    /// Add a payload and return completed block packets (if block is full).
    ///
    /// Returns `Err` if the payload exceeds `MAX_SHARD_PAYLOAD`.
    fn add_payload(&mut self, payload: &[u8]) -> Result<Option<Vec<Vec<u8>>>, io::Error> {
        if payload.len() > MAX_SHARD_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "FEC payload too large: {} bytes exceeds maximum of {}",
                    payload.len(),
                    MAX_SHARD_PAYLOAD
                ),
            ));
        }
        let len = payload.len();
        self.shard_buf.push(payload[..len].to_vec());
        self.shard_lens.push(len);

        if self.shard_buf.len() == self.data_shards {
            Ok(Some(self.flush_block()))
        } else {
            Ok(None)
        }
    }

    /// Flush whatever data shards we have (possibly < data_shards) into a complete FEC block.
    fn flush(&mut self) -> Option<Vec<Vec<u8>>> {
        if self.shard_buf.is_empty() {
            return None;
        }
        // Pad with empty shards if we have fewer than data_shards.
        while self.shard_buf.len() < self.data_shards {
            self.shard_buf.push(Vec::new());
            self.shard_lens.push(0);
        }
        Some(self.flush_block())
    }

    fn flush_block(&mut self) -> Vec<Vec<u8>> {
        let block_id = self.block_id;
        self.block_id = self.block_id.wrapping_add(1);

        // Prepend 2-byte original payload length to each data shard.
        // This embeds the length in the RS-coded data so that after
        // reconstruction of lost shards the original size is recoverable.
        let mut shards: Vec<Vec<u8>> = self
            .shard_buf
            .drain(..)
            .enumerate()
            .map(|(i, s)| {
                let orig_len = self.shard_lens[i] as u16;
                let mut prefixed = Vec::with_capacity(2 + s.len());
                prefixed.extend_from_slice(&orig_len.to_be_bytes());
                prefixed.extend(s);
                prefixed
            })
            .collect();

        // Normalize shard sizes (Reed-Solomon requires equal-length shards).
        let max_len = shards.iter().map(|s| s.len()).max().unwrap_or(0).max(1);
        for shard in &mut shards {
            shard.resize(max_len, 0);
        }

        // Add empty parity shards.
        for _ in 0..self.parity_shards {
            shards.push(vec![0u8; max_len]);
        }

        // Encode parity.
        self.rs.encode(&mut shards).expect("FEC encode failed");

        let total = self.data_shards + self.parity_shards;
        let mut packets = Vec::with_capacity(total);

        for (idx, shard) in shards.into_iter().enumerate() {
            let payload_len = shard.len() as u16;

            let mut pkt = Vec::with_capacity(FEC_HEADER_SIZE + shard.len());
            pkt.extend_from_slice(&block_id.to_be_bytes());
            pkt.push(idx as u8);
            pkt.push(self.data_shards as u8);
            pkt.push(self.parity_shards as u8);
            pkt.extend_from_slice(&payload_len.to_be_bytes());
            pkt.extend_from_slice(&shard);
            packets.push(pkt);
        }

        self.shard_lens.clear();
        packets
    }
}

/// A received FEC block being reassembled.
struct FecBlock {
    data_shards: usize,
    parity_shards: usize,
    shard_size: Option<usize>,
    /// `None` = not yet received.
    shards: Vec<Option<Vec<u8>>>,
    received_count: usize,
}

impl FecBlock {
    fn new(data_shards: usize, parity_shards: usize) -> Self {
        let total = data_shards + parity_shards;
        Self {
            data_shards,
            parity_shards,
            shard_size: None,
            shards: vec![None; total],
            received_count: 0,
        }
    }

    /// Insert a shard. Returns true if the block is now reconstructable.
    fn insert(&mut self, idx: usize, _payload_len: u16, data: Vec<u8>) -> bool {
        let total = self.data_shards + self.parity_shards;
        if idx >= total || self.shards[idx].is_some() {
            return self.received_count >= self.data_shards;
        }

        if self.shard_size.is_none() {
            self.shard_size = Some(data.len());
        }

        // Normalize size.
        let expected = self.shard_size.unwrap();
        let mut data = data;
        data.resize(expected, 0);

        self.shards[idx] = Some(data);
        self.received_count += 1;

        self.received_count >= self.data_shards
    }

    /// Attempt to reconstruct missing data shards and return the original payloads.
    fn reconstruct(mut self) -> Option<Vec<Vec<u8>>> {
        if self.received_count < self.data_shards {
            return None;
        }

        let rs = ReedSolomon::new(self.data_shards, self.parity_shards).ok()?;

        // Only reconstruct if some data shards are missing.
        let data_missing = self.shards[..self.data_shards].iter().any(|s| s.is_none());
        if data_missing {
            if rs.reconstruct(&mut self.shards).is_err() {
                tracing::warn!("FEC reconstruction failed");
                return None;
            }
        }

        let mut result = Vec::with_capacity(self.data_shards);
        for i in 0..self.data_shards {
            let shard = self.shards[i].take()?;
            if shard.len() < 2 {
                continue;
            }
            // Extract the original payload length from the embedded 2-byte prefix.
            // This works for both directly-received and RS-reconstructed shards.
            let original_len = u16::from_be_bytes([shard[0], shard[1]]) as usize;
            if original_len == 0 {
                // Padding shard from incomplete block — skip.
                continue;
            }
            let end = (2 + original_len).min(shard.len());
            result.push(shard[2..end].to_vec());
        }

        Some(result)
    }
}

/// Composite key for FEC blocks: `(source_address, block_id)`.
///
/// On the server side, multiple clients share a single `FecTransport`.
/// Without the address component, two clients starting with `block_id = 0`
/// would have their shards mixed into the same `FecBlock`, producing
/// garbage reconstructions.
type BlockKey = (SocketAddr, u32);

/// Decoder state for incoming packets.
struct FecDecoder {
    /// In-progress blocks keyed by (source address, block_id).
    blocks: HashMap<BlockKey, FecBlock>,
    /// Completed datagrams ready to be delivered.
    ready: std::collections::VecDeque<(Vec<u8>, SocketAddr)>,
    /// Per-source maximum block_id, for sliding-window eviction.
    max_block_ids: HashMap<SocketAddr, u32>,
    /// Already-completed block keys, to prevent duplicate delivery when
    /// late packets for an already-reconstructed block arrive.
    completed: HashSet<BlockKey>,
}

impl FecDecoder {
    fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            ready: std::collections::VecDeque::new(),
            max_block_ids: HashMap::new(),
            completed: HashSet::new(),
        }
    }

    /// Feed a raw FEC packet received from the network.
    fn feed(&mut self, raw: &[u8], addr: SocketAddr) {
        if raw.len() < FEC_HEADER_SIZE {
            return;
        }

        let block_id = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let shard_idx = raw[4] as usize;
        let data_shards = raw[5] as usize;
        let parity_shards = raw[6] as usize;
        let payload_len = u16::from_be_bytes([raw[7], raw[8]]);
        let shard_data = raw[FEC_HEADER_SIZE..].to_vec();

        if data_shards == 0 || parity_shards == 0 {
            return;
        }

        let key = (addr, block_id);

        // Skip already-completed blocks to prevent duplicate delivery.
        if self.completed.contains(&key) {
            return;
        }

        // Evict very old blocks (sliding window).
        let max = self.max_block_ids.entry(addr).or_insert(0);
        if block_id.wrapping_sub(*max) < u32::MAX / 2 {
            *max = block_id;
        }
        self.evict_old_blocks();

        let block = self.blocks.entry(key).or_insert_with(|| FecBlock::new(data_shards, parity_shards));

        if block.insert(shard_idx, payload_len, shard_data) {
            // Block is complete — reconstruct and deliver.
            if let Some(block) = self.blocks.remove(&key) {
                self.completed.insert(key);
                if let Some(payloads) = block.reconstruct() {
                    for payload in payloads {
                        self.ready.push_back((payload, addr));
                    }
                }
            }
        }
    }

    fn evict_old_blocks(&mut self) {
        // Keep at most 256 in-progress blocks per source.
        if self.blocks.len() > 256 {
            let max_ids = &self.max_block_ids;
            self.blocks.retain(|&(addr, id), _| {
                let max = max_ids.get(&addr).copied().unwrap_or(0);
                let threshold = max.wrapping_sub(128);
                id.wrapping_sub(threshold) < 256
            });
        }
        // Evict old completed-block keys to bound memory usage.
        if self.completed.len() > 512 {
            let max_ids = &self.max_block_ids;
            self.completed.retain(|&(addr, id)| {
                let max = max_ids.get(&addr).copied().unwrap_or(0);
                let threshold = max.wrapping_sub(256);
                id.wrapping_sub(threshold) < 512
            });
        }
    }

    fn pop_ready(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        self.ready.pop_front()
    }
}

/// A [`Transport`] wrapper that applies Reed-Solomon FEC at the UDP packet level.
///
/// This sits between KCP and the underlying UDP transport, transparently
/// encoding outgoing packets with parity data and reconstructing lost
/// incoming packets.
///
/// FEC encoders are maintained per target address to prevent data from
/// different clients from being mixed into the same FEC block.
///
/// Incomplete FEC blocks are automatically flushed after [`FLUSH_TIMEOUT`]
/// to prevent tail-latency stalls.
pub struct FecTransport<T: Transport<Addr = SocketAddr>> {
    inner: T,
    data_shards: usize,
    parity_shards: usize,
    /// Per-target FEC encoders.
    encoders: Arc<Mutex<HashMap<SocketAddr, FecEncoder>>>,
    decoder: Arc<Mutex<FecDecoder>>,
    /// Per-target timestamps of the last buffered (but not yet flushed) send.
    pending_flush: Arc<Mutex<HashMap<SocketAddr, Instant>>>,
}

impl<T: Transport<Addr = SocketAddr>> FecTransport<T> {
    /// Wrap an existing transport with FEC.
    pub fn new(inner: T, config: &FecConfig) -> Self {
        Self {
            inner,
            data_shards: config.data_shards,
            parity_shards: config.parity_shards,
            encoders: Arc::new(Mutex::new(HashMap::new())),
            decoder: Arc::new(Mutex::new(FecDecoder::new())),
            pending_flush: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Flush any per-target encoders whose timeout has expired.
    async fn do_flush(&self) -> io::Result<()> {
        let flush_targets: Vec<SocketAddr> = {
            let pending = self.pending_flush.lock().await;
            pending
                .iter()
                .filter(|(_, ts)| ts.elapsed() >= FLUSH_TIMEOUT)
                .map(|(addr, _)| *addr)
                .collect()
        };

        for target in &flush_targets {
            let packets = {
                let mut encoders = self.encoders.lock().await;
                if let Some(encoder) = encoders.get_mut(target) {
                    encoder.flush()
                } else {
                    None
                }
            };
            {
                let mut pending = self.pending_flush.lock().await;
                pending.remove(target);
            }
            if let Some(packets) = packets {
                for pkt in &packets {
                    self.inner.send_to(pkt, target).await?;
                }
            }
        }

        Ok(())
    }
}

impl<T: Transport<Addr = SocketAddr>> Transport for FecTransport<T> {
    type Addr = SocketAddr;

    async fn send_to(&self, buf: &[u8], target: &SocketAddr) -> io::Result<usize> {
        let packets = {
            let mut encoders = self.encoders.lock().await;
            let encoder = encoders
                .entry(*target)
                .or_insert_with(|| FecEncoder::new(self.data_shards, self.parity_shards));
            encoder.add_payload(buf)?
        };

        if let Some(packets) = packets {
            // Full block — send immediately and clear pending for this target.
            {
                let mut pending = self.pending_flush.lock().await;
                pending.remove(target);
            }
            for pkt in &packets {
                self.inner.send_to(pkt, target).await?;
            }
        } else {
            // Incomplete block — record target and time for auto-flush.
            let mut pending = self.pending_flush.lock().await;
            pending.insert(*target, Instant::now());
        }

        Ok(buf.len())
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            // Check if we have already-decoded data ready.
            {
                let mut decoder = self.decoder.lock().await;
                if let Some((data, addr)) = decoder.pop_ready() {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    return Ok((n, addr));
                }
            }

            // Determine the nearest flush deadline across all targets.
            let flush_deadline = {
                let pending = self.pending_flush.lock().await;
                pending.values().map(|ts| *ts + FLUSH_TIMEOUT).min()
            };

            // Receive a raw packet from the underlying transport,
            // but also auto-flush encoders if a timeout expires.
            let mut raw_buf = vec![0u8; MAX_SHARD_PAYLOAD + FEC_HEADER_SIZE + 100];
            let recv_result = if let Some(deadline) = flush_deadline {
                let sleep_dur = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    result = self.inner.recv_from(&mut raw_buf) => Some(result),
                    _ = tokio::time::sleep(sleep_dur) => {
                        self.do_flush().await?;
                        None
                    }
                }
            } else {
                Some(self.inner.recv_from(&mut raw_buf).await)
            };

            let Some(recv_result) = recv_result else { continue };
            let (n, addr) = recv_result?;
            let raw = &raw_buf[..n];

            // Feed into decoder.
            let mut decoder = self.decoder.lock().await;
            decoder.feed(raw, addr);

            if let Some((data, addr)) = decoder.pop_ready() {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                return Ok((n, addr));
            }
            // No complete block yet — loop and receive more packets.
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Flush any pending partial FEC blocks for a specific target.
///
/// This is now handled automatically by [`FecTransport`] using a timeout,
/// but can still be called manually for immediate flushing.
pub async fn flush_fec_encoder<T: Transport<Addr = SocketAddr>>(
    transport: &FecTransport<T>,
    target: &SocketAddr,
) -> io::Result<()> {
    let packets = {
        let mut encoders = transport.encoders.lock().await;
        if let Some(encoder) = encoders.get_mut(target) {
            encoder.flush()
        } else {
            None
        }
    };
    {
        let mut pending = transport.pending_flush.lock().await;
        pending.remove(target);
    }
    if let Some(packets) = packets {
        for pkt in &packets {
            transport.inner.send_to(pkt, target).await?;
        }
    }
    Ok(())
}

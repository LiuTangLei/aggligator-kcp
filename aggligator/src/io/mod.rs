//! Wrapper types for stream-based links.
//!
//! These wrapper types turn stream-based links into packet-based links
//! by applying the [integrity codec](IntegrityCodec).
//!
//! They are applied by the [`Server::add_incoming_io`](crate::connect::Server::add_incoming_io)
//! and [`Control::add_io`](crate::control::Control::add_io) methods to stream-based links,
//! using the default configuration of the integrity codec.
//!

mod codec;

use byteorder::{WriteBytesExt, BE};
use bytes::Bytes;
use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::{msg::LinkMsg, protocol_err, seq::Seq};

pub use codec::*;

const DATAGRAM_MAGIC: &[u8; 4] = b"AGD1";
const DATAGRAM_KIND_CONTROL: u8 = 0;
const DATAGRAM_KIND_DATA: u8 = 1;
const DATAGRAM_CONTROL_HEADER_LEN: usize = DATAGRAM_MAGIC.len() + 1;
const DATAGRAM_DATA_HEADER_LEN: usize = DATAGRAM_CONTROL_HEADER_LEN + 4;

/// Transmit wrapper for using an IO-stream-based link.
#[derive(Debug)]
pub struct IoTx<W>(pub FramedWrite<W, IntegrityCodec>);

impl<W> IoTx<W>
where
    W: AsyncWrite,
{
    /// Wraps an IO writer using the default configuration of the integrity codec.
    pub fn new(write: W) -> Self {
        Self(FramedWrite::new(write, IntegrityCodec::new()))
    }

    /// Wraps an IO writer using a customized integrity codec.
    pub fn with_codec(write: W, codec: IntegrityCodec) -> Self {
        Self(FramedWrite::new(write, codec))
    }
}

impl<W> Sink<Bytes> for IoTx<W>
where
    W: AsyncWrite + Unpin,
{
    type Error = io::Error;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_ready_unpin(cx)
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        Pin::into_inner(self).0.start_send_unpin(item)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_close_unpin(cx)
    }
}

/// Receive wrapper for using an IO-stream-based link.
#[derive(Debug)]
pub struct IoRx<R>(pub FramedRead<R, IntegrityCodec>);

impl<R> IoRx<R>
where
    R: AsyncRead,
{
    /// Wraps an IO reader using the default configuration of the integrity codec.
    pub fn new(read: R) -> Self {
        Self(FramedRead::new(read, IntegrityCodec::new()))
    }

    /// Wraps an IO reader using a customized integrity codec.
    pub fn with_codec(read: R, codec: IntegrityCodec) -> Self {
        Self(FramedRead::new(read, codec))
    }
}

impl<R> Stream for IoRx<R>
where
    R: AsyncRead + Unpin,
{
    type Item = Result<Bytes, io::Error>;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::into_inner(self).0.poll_next_unpin(cx).map_ok(|v| v.freeze())
    }
}

/// Type-neutral transmit wrapper for using an IO-stream-based link.
///
/// Useful if a connection consists of different types of links.
pub type IoTxBox = IoTx<Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>>;

/// Type-neutral receive wrapper for using an IO-stream-based link.
///
/// Useful if a connection consists of different types of links.
pub type IoRxBox = IoRx<Pin<Box<dyn AsyncRead + Send + Sync + 'static>>>;

/// A stream, either packet-based or IO-based.
pub enum StreamBox {
    /// Packet-based stream.
    TxRx(TxRxBox),
    /// IO-based stream.
    Io(IoBox),
    /// Unreliable datagram-based stream.
    ///
    /// This stream preserves packet boundaries, but may lose, duplicate, or
    /// reorder packets. It is adapted into the regular link stream by making
    /// reliable data messages self-contained inside a single datagram.
    UnreliableDatagram(DatagramBox),
}

impl StreamBox {
    /// Make stream packet-based.
    ///
    /// A packet-based stream is unaffacted.
    /// An IO-based stream is wrapped in the integrity codec.
    pub fn into_tx_rx(self) -> TxRxBox {
        match self {
            Self::TxRx(tx_rx) => tx_rx,
            Self::Io(IoBox { read, write }) => {
                let tx = IoTxBox::new(write);
                let rx = IoRxBox::new(read);
                TxRxBox::new(tx, rx)
            }
            Self::UnreliableDatagram(datagram) => datagram.into_tx_rx(),
        }
    }
}

impl From<TxRxBox> for StreamBox {
    fn from(value: TxRxBox) -> Self {
        Self::TxRx(value)
    }
}

impl From<IoBox> for StreamBox {
    fn from(value: IoBox) -> Self {
        Self::Io(value)
    }
}

impl From<DatagramBox> for StreamBox {
    fn from(value: DatagramBox) -> Self {
        Self::UnreliableDatagram(value)
    }
}

pub(crate) type TxBox = Pin<Box<dyn Sink<Bytes, Error = io::Error> + Send + Sync + 'static>>;
pub(crate) type RxBox = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send + Sync + 'static>>;

/// A boxed packet-based stream.
pub struct TxRxBox {
    /// Sender.
    pub tx: TxBox,
    /// Receiver.
    pub rx: RxBox,
}

impl TxRxBox {
    /// Creates a new instance.
    pub fn new(
        tx: impl Sink<Bytes, Error = io::Error> + Send + Sync + 'static,
        rx: impl Stream<Item = io::Result<Bytes>> + Send + Sync + 'static,
    ) -> Self {
        Self { tx: Box::pin(tx), rx: Box::pin(rx) }
    }

    /// Splits this into boxed transmitter and receiver.
    pub fn into_split(self) -> (TxBox, RxBox) {
        let Self { tx, rx } = self;
        (tx, rx)
    }
}

impl Sink<Bytes> for TxRxBox {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.get_mut().tx.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> io::Result<()> {
        self.get_mut().tx.start_send_unpin(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.get_mut().tx.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.get_mut().tx.poll_close_unpin(cx)
    }
}

impl Stream for TxRxBox {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_next_unpin(cx)
    }
}

/// A boxed unreliable datagram stream.
///
/// Each item sent to `tx` must be emitted as one datagram by the underlying
/// transport. The transport may lose, duplicate, or reorder datagrams.
pub struct DatagramBox {
    /// Sender.
    pub tx: TxBox,
    /// Receiver.
    pub rx: RxBox,
}

impl DatagramBox {
    /// Creates a new instance.
    pub fn new(
        tx: impl Sink<Bytes, Error = io::Error> + Send + Sync + 'static,
        rx: impl Stream<Item = io::Result<Bytes>> + Send + Sync + 'static,
    ) -> Self {
        Self { tx: Box::pin(tx), rx: Box::pin(rx) }
    }

    /// Splits this into boxed transmitter and receiver.
    pub fn into_split(self) -> (TxBox, RxBox) {
        let Self { tx, rx } = self;
        (tx, rx)
    }

    /// Adapts this unreliable datagram stream into the regular link stream.
    pub fn into_tx_rx(self) -> TxRxBox {
        let (tx, rx) = self.into_split();
        TxRxBox::new(DatagramTx::new(tx), DatagramRx::new(rx))
    }
}

/// Transmit adapter for unreliable datagram links.
struct DatagramTx {
    tx: TxBox,
    pending_data_seq: Option<Seq>,
}

impl DatagramTx {
    fn new(tx: TxBox) -> Self {
        Self { tx, pending_data_seq: None }
    }

    fn encode_control(msg: Bytes) -> Bytes {
        let mut datagram = Vec::with_capacity(DATAGRAM_CONTROL_HEADER_LEN + msg.len());
        datagram.extend_from_slice(DATAGRAM_MAGIC);
        datagram.push(DATAGRAM_KIND_CONTROL);
        datagram.extend_from_slice(&msg);
        datagram.into()
    }

    fn encode_data(seq: Seq, payload: Bytes) -> io::Result<Bytes> {
        let mut datagram = Vec::with_capacity(DATAGRAM_DATA_HEADER_LEN + payload.len());
        datagram.extend_from_slice(DATAGRAM_MAGIC);
        datagram.push(DATAGRAM_KIND_DATA);
        datagram.write_u32::<BE>(seq.into())?;
        datagram.extend_from_slice(&payload);
        Ok(datagram.into())
    }
}

impl Sink<Bytes> for DatagramTx {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.tx.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> io::Result<()> {
        if let Some(seq) = self.pending_data_seq.take() {
            let datagram = Self::encode_data(seq, item)?;
            return self.tx.start_send_unpin(datagram);
        }

        match LinkMsg::read(item.as_ref())? {
            LinkMsg::Data { seq } => {
                self.pending_data_seq = Some(seq);
                Ok(())
            }
            _ => self.tx.start_send_unpin(Self::encode_control(item)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.pending_data_seq.is_some() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data message header was not followed by payload",
            )));
        }
        self.tx.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.pending_data_seq.is_some() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data message header was not followed by payload",
            )));
        }
        self.tx.poll_close_unpin(cx)
    }
}

/// Receive adapter for unreliable datagram links.
struct DatagramRx {
    rx: RxBox,
    pending_payload: Option<Bytes>,
}

impl DatagramRx {
    fn new(rx: RxBox) -> Self {
        Self { rx, pending_payload: None }
    }

    fn decode(datagram: Bytes) -> io::Result<DecodedDatagram> {
        if datagram.len() < DATAGRAM_CONTROL_HEADER_LEN || &datagram[..DATAGRAM_MAGIC.len()] != DATAGRAM_MAGIC {
            return Err(protocol_err!("invalid datagram link frame"));
        }

        match datagram[DATAGRAM_MAGIC.len()] {
            DATAGRAM_KIND_CONTROL => Ok(DecodedDatagram::Control(datagram.slice(DATAGRAM_CONTROL_HEADER_LEN..))),
            DATAGRAM_KIND_DATA => {
                if datagram.len() < DATAGRAM_DATA_HEADER_LEN {
                    return Err(protocol_err!("data datagram link frame too short"));
                }
                let seq = u32::from_be_bytes(
                    datagram[DATAGRAM_CONTROL_HEADER_LEN..DATAGRAM_DATA_HEADER_LEN].try_into().unwrap(),
                )
                .into();
                Ok(DecodedDatagram::Data { seq, payload: datagram.slice(DATAGRAM_DATA_HEADER_LEN..) })
            }
            other => Err(protocol_err!("invalid datagram link frame kind {other}")),
        }
    }
}

enum DecodedDatagram {
    Control(Bytes),
    Data { seq: Seq, payload: Bytes },
}

impl Stream for DatagramRx {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if let Some(payload) = self.pending_payload.take() {
            return Poll::Ready(Some(Ok(payload)));
        }

        match self.rx.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(Some(Ok(datagram))) => match Self::decode(datagram) {
                Ok(DecodedDatagram::Control(msg)) => Poll::Ready(Some(Ok(msg))),
                Ok(DecodedDatagram::Data { seq, payload }) => {
                    self.pending_payload = Some(payload);
                    Poll::Ready(Some(Ok(LinkMsg::Data { seq }.encode())))
                }
                Err(err) => Poll::Ready(Some(Err(err))),
            },
        }
    }
}

pub(crate) type ReadBox = Pin<Box<dyn AsyncRead + Send + Sync + 'static>>;
pub(crate) type WriteBox = Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>;

/// A boxed IO stream.
pub struct IoBox {
    /// Reader.
    pub read: ReadBox,
    /// Writer.
    pub write: WriteBox,
}

impl IoBox {
    /// Creates a new instance.
    pub fn new(
        read: impl AsyncRead + Send + Sync + 'static, write: impl AsyncWrite + Send + Sync + 'static,
    ) -> Self {
        Self { read: Box::pin(read), write: Box::pin(write) }
    }

    /// Splits this into boxed reader and writer.
    pub fn into_split(self) -> (ReadBox, WriteBox) {
        let Self { read, write } = self;
        (read, write)
    }
}

impl AsyncRead for IoBox {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl AsyncWrite for IoBox {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_control_frame_roundtrips() {
        let msg = LinkMsg::Ping.encode();
        let datagram = DatagramTx::encode_control(msg.clone());

        match DatagramRx::decode(datagram).unwrap() {
            DecodedDatagram::Control(decoded) => assert_eq!(decoded, msg),
            DecodedDatagram::Data { .. } => panic!("expected control datagram"),
        }
    }

    #[test]
    fn datagram_data_frame_roundtrips() {
        let payload = Bytes::from_static(b"hello over udp");
        let datagram = DatagramTx::encode_data(Seq::from(42), payload.clone()).unwrap();

        match DatagramRx::decode(datagram).unwrap() {
            DecodedDatagram::Data { seq, payload: decoded } => {
                assert_eq!(seq, Seq::from(42));
                assert_eq!(decoded, payload);
            }
            DecodedDatagram::Control(_) => panic!("expected data datagram"),
        }
    }
}

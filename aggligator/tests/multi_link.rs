//! Multi-link tests.

use bytes::Bytes;
use futures::{future, join};
use std::{
    future::IntoFuture,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};
use tokio::sync::oneshot;

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::test_data::send_and_verify;
use aggligator::{
    alc::{RecvError, SendError},
    cfg::{AggMode, Cfg, LinkPing},
    connect::{connect, Server},
    control::{DisconnectReason, Stats},
    exec::{
        self,
        time::{sleep, timeout, Instant},
    },
    io::{DatagramBox, TxRxBox},
    TaskError,
};

mod test_channel;
mod test_data;

#[derive(Debug, Clone, Default)]
struct LinkDesc {
    cfg: test_channel::Cfg,
    pause: Option<(usize, Duration)>,
    fail: Option<usize>,
    block: Option<(usize, Duration)>,
}

#[derive(Debug)]
struct LossRun {
    stats: Stats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LatencySimMode {
    TcpLikeBandwidth,
    DatagramBandwidth,
    DatagramBandwidthRedundant,
    DatagramLowLatency,
}

impl LatencySimMode {
    fn agg_mode(self) -> AggMode {
        match self {
            Self::TcpLikeBandwidth | Self::DatagramBandwidth => AggMode::Bandwidth,
            Self::DatagramBandwidthRedundant => AggMode::BandwidthRedundant,
            Self::DatagramLowLatency => AggMode::LowLatency,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::TcpLikeBandwidth => "tcp-like bandwidth",
            Self::DatagramBandwidth => "datagram bandwidth",
            Self::DatagramBandwidthRedundant => "datagram bw redundant",
            Self::DatagramLowLatency => "datagram low-latency",
        }
    }

    fn is_datagram(self) -> bool {
        !matches!(self, Self::TcpLikeBandwidth)
    }
}

#[derive(Debug)]
struct LatencyRun {
    mode: LatencySimMode,
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
    stats: Stats,
}

#[derive(Debug, Clone, Copy)]
struct AsymmetricLossProfile {
    fast_loss_every: usize,
    slow_loss_every: usize,
}

impl AsymmetricLossProfile {
    fn fast_loss_percent(self) -> f64 {
        loss_percent(self.fast_loss_every)
    }

    fn slow_loss_percent(self) -> f64 {
        loss_percent(self.slow_loss_every)
    }
}

impl LatencyRun {
    fn print(&self) {
        println!(
            "{:<22} p50={:>6.1}ms p95={:>6.1}ms p99={:>6.1}ms max={:>6.1}ms overhead={}%, hedge={}/{}, dup={}",
            self.mode.label(),
            duration_ms(self.p50),
            duration_ms(self.p95),
            duration_ms(self.p99),
            duration_ms(self.max),
            self.stats.redundancy_overhead_percent,
            self.stats.hedge_won,
            self.stats.hedge_sent,
            self.stats.duplicate_data_received,
        );
    }
}

async fn multi_link_test(
    link_descs: &[LinkDesc], cfg: Cfg, max_size: usize, count: usize, expected_speed: usize, should_fail: bool,
    terminate: Option<usize>,
) {
    let mut server_links = Vec::new();
    let mut client_links = Vec::new();
    let mut a_controls = Vec::new();
    let mut b_controls = Vec::new();

    for ld in link_descs {
        let (link_a_tx, link_a_rx, link_a_control) = test_channel::channel(ld.cfg.clone());
        let (link_b_tx, link_b_rx, link_b_control) = test_channel::channel(ld.cfg.clone());

        server_links.push((link_a_rx, link_b_tx));
        client_links.push((link_b_rx, link_a_tx));
        a_controls.push(link_a_control);
        b_controls.push(link_b_control);
    }

    let server_cfg = cfg.clone();
    let server_task = async move {
        println!("server: starting");
        let server = Server::new(server_cfg);

        println!("server: obtaining listener");
        let mut listener = server.listen().unwrap();

        let mut added_links = Vec::new();
        for (n, (rx, tx)) in server_links.into_iter().enumerate() {
            println!("server: adding incoming link {n}");
            added_links.push(server.add_incoming(tx, rx, format!("{n}"), &[]).await.unwrap());
        }

        println!("server: getting incoming connection");
        let mut incoming = listener.next().await.unwrap();

        let link_names = incoming.link_tags();
        println!("server: links of incoming connection: {link_names:?}");
        assert_eq!(link_names.len(), added_links.len());
        for n in 0..added_links.len() {
            assert!(link_names.iter().any(|name| name.as_str() == format!("{n}")));
        }

        println!("server: accepting incoming connection");
        let (task, ch, mut control) = incoming.accept();
        let task = exec::spawn(task.into_future());
        assert!(!control.is_terminated());

        println!("server: waiting for links");
        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let links = control.links();
        assert_eq!(links.len(), added_links.len());
        for n in 0..added_links.len() {
            assert!(links.iter().any(|link| link.tag().as_str() == format!("{n}")));
        }
        for link in links {
            assert!(!link.is_disconnected());
            assert!(link.disconnect_reason().is_none());
        }

        println!("server: sending and receiving test data");
        let (tx, mut rx) = ch.into_tx_rx();
        println!("server: maximum send size is {}", tx.max_size());

        let expected_send_err = match (should_fail, terminate) {
            (true, _) => Some(SendError::AllLinksFailed),
            (_, Some(_)) => Some(SendError::TaskTerminated),
            _ => None,
        };
        let expected_recv_err = match (should_fail, terminate) {
            (true, _) => Some(RecvError::AllLinksFailed),
            (_, Some(_)) => Some(RecvError::TaskTerminated),
            _ => None,
        };
        let speed = send_and_verify(
            "server",
            &tx,
            &mut rx,
            0,
            tx.max_size().min(max_size),
            count,
            |i| {
                for (n, desc) in link_descs.iter().enumerate() {
                    if let Some((when, dur)) = desc.pause {
                        if i == when {
                            println!("pausing link a {n}");
                            let ctrl = a_controls[n].clone();
                            exec::spawn(async move {
                                let _ = ctrl.pause_for(dur).await;
                                println!("unpausing link a {n}");
                            });
                        }
                    }
                    if let Some(when) = desc.fail {
                        if i == when {
                            println!("failing link a {n}");
                            let ctrl = a_controls[n].clone();
                            exec::spawn(async move { ctrl.disconnect().await });
                        }
                    }
                    if let Some((when, dur)) = desc.block {
                        if i == when {
                            println!("blocking link a {n}");
                            let link = added_links[n].clone();
                            link.set_blocked(true);
                            assert!(link.is_blocked());
                            exec::spawn(async move {
                                sleep(dur).await;
                                println!("unblocking link a {n}");
                                link.set_blocked(false);
                                assert!(!link.is_blocked());
                            });
                        }
                    }
                }
            },
            expected_send_err,
            expected_recv_err,
        )
        .await;

        println!("server: measured speed is {speed:.1} and expected speed is {expected_speed:.1}");
        #[cfg(not(debug_assertions))]
        if terminate.is_none() {
            assert!(speed as usize >= expected_speed, "server too slow");
        }

        for (n, (link, desc)) in added_links.iter().zip(link_descs).enumerate() {
            println!("server: link status {n}: {:?}", link.disconnect_reason());
            if desc.fail.is_some() {
                if !link.is_disconnected() {
                    println!("server: waiting for link {n} disconnect");
                }
                link.disconnected().await;
                match link.disconnect_reason() {
                    Some(reason) if reason.should_reconnect() => (),
                    other => panic!("no or wrong disconnect reason: {other:?}"),
                }
            } else if terminate.is_some() {
                if !link.is_disconnected() {
                    println!("server: waiting for link {n} disconnect");
                }
                link.disconnected().await;
                assert!(matches!(link.disconnect_reason(), Some(DisconnectReason::TaskTerminated)));
            } else {
                assert!(!link.is_disconnected());
            }
        }

        println!("server: dropping sender");
        drop(tx);

        if !should_fail && terminate.is_none() {
            println!("server: waiting for receive end");
            assert_eq!(rx.recv().await.unwrap(), None);
        }

        println!("server: waiting for termination notification");
        let result = control.terminated().await;
        if should_fail || terminate.is_some() {
            result.expect_err("control did not fail");
        } else {
            result.expect("control failed");
        }
        assert!(control.is_terminated());

        for (n, link) in added_links.iter().enumerate() {
            println!("server: waiting for link disconnect notification {n}");
            link.disconnected().await;
            let stats = link.stats();
            println!("server: Link {n} stats: {stats:?}");
        }

        println!("server: waiting for task termination");
        let result = task.await.unwrap();
        if !should_fail && terminate.is_none() {
            result.expect("server task failed");
            println!("server: done");
        } else {
            let err = result.expect_err("server task did not fail");
            println!("server error: {err}");
            if terminate.is_some() {
                assert!(matches!(err, TaskError::Terminated));
            }
        }
    };

    let client_task = async move {
        println!("client: starting outgoing link");
        let (task, outgoing, mut control) = connect(cfg);
        let task = exec::spawn(task.into_future());

        let mut added_links_tasks = Vec::new();
        for (n, (rx, tx)) in client_links.into_iter().enumerate() {
            println!("client: adding outgoing link {n}");
            added_links_tasks.push(control.add(tx, rx, format!("{n}"), &[]));
        }
        let added_links = future::try_join_all(added_links_tasks).await.unwrap();

        println!("client: waiting for links");
        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        println!("client: checking link info");
        let links = control.links();
        println!("client: links of outgoing connection: {links:?}");
        assert_eq!(links.len(), added_links.len());
        for n in 0..added_links.len() {
            assert!(links.iter().any(|link| link.tag().as_str() == format!("{n}")));
        }
        for link in links {
            assert!(!link.is_disconnected());
            assert!(link.disconnect_reason().is_none());
        }

        println!("client: establishing connection");
        let ch = outgoing.connect().await.unwrap();

        println!("client: sending and receiving test data");
        let (tx, mut rx) = ch.into_tx_rx();

        let expected_send_err = match (should_fail, terminate) {
            (true, _) => Some(SendError::AllLinksFailed),
            (_, Some(_)) => Some(SendError::TaskTerminated),
            _ => None,
        };
        let expected_recv_err = match (should_fail, terminate) {
            (true, _) => Some(RecvError::AllLinksFailed),
            (_, Some(_)) => Some(RecvError::TaskTerminated),
            _ => None,
        };
        let speed = send_and_verify(
            "client",
            &tx,
            &mut rx,
            0,
            tx.max_size().min(max_size),
            count,
            |i| {
                for (n, desc) in link_descs.iter().enumerate() {
                    if let Some((when, dur)) = desc.pause {
                        if i == when {
                            println!("pausing link b {n}");
                            let ctrl = b_controls[n].clone();
                            exec::spawn(async move {
                                let _ = ctrl.pause_for(dur).await;
                                println!("unpausing link b {n}");
                            });
                        }
                    }
                    if let Some(when) = desc.fail {
                        if i == when {
                            println!("failing link b {n}");
                            let ctrl = b_controls[n].clone();
                            exec::spawn(async move { ctrl.disconnect().await });
                        }
                    }
                }

                match terminate {
                    Some(terminate) if terminate == i => {
                        println!("client: forcefully terminating connection");
                        control.terminate();
                    }
                    _ => (),
                }
            },
            expected_send_err,
            expected_recv_err,
        )
        .await;

        println!("client: measured speed is {speed:.1} and expected speed is {expected_speed:.1}");
        #[cfg(not(debug_assertions))]
        if terminate.is_none() {
            assert!(speed as usize >= expected_speed, "client too slow");
        }

        println!("client: dropping sender");
        drop(tx);

        println!("client: waiting for termination notification");
        let result = control.terminated().await;
        if should_fail || terminate.is_some() {
            result.expect_err("control did not fail");
        } else {
            result.expect("control failed");
        }
        assert!(control.is_terminated());

        println!("client: waiting for task termination");
        let result = task.await.unwrap();
        if !should_fail && terminate.is_none() {
            result.expect("client task failed");
            println!("client: task done");
        } else {
            let err = result.expect_err("client task did not fail");
            println!("client error: {err}");
            if terminate.is_some() {
                assert!(matches!(err, TaskError::Terminated));
            }
        }

        for (n, (link, desc)) in added_links.iter().zip(link_descs).enumerate() {
            println!("client: link status {n} (name: {}): {:?}", link.tag(), link.disconnect_reason());
            if desc.fail.is_some() {
                if !link.is_disconnected() {
                    println!("client: waiting for link {n} disconnect");
                }
                link.disconnected().await;
                match link.disconnect_reason() {
                    Some(reason) if reason.should_reconnect() => (),
                    Some(DisconnectReason::ConnectionClosed) => (),
                    other => panic!("no or wrong disconnect reason: {other:?}"),
                }
            } else if terminate.is_some() {
                if !link.is_disconnected() {
                    println!("client: waiting for link {n} disconnect");
                }
                link.disconnected().await;
                assert!(matches!(link.disconnect_reason(), Some(DisconnectReason::TaskTerminated)));
            }
        }

        for (n, link) in added_links.iter().enumerate() {
            println!("client: waiting for link disconnect notification {n}");
            link.disconnected().await;
            let stats = link.stats();
            println!("client: Link {n} stats: {stats:?}");
        }

        println!("client: done");
    };

    join!(server_task, client_task);
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn five_x_unlimited_multi_thread() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg { speed: 0, latency: None, ..Default::default() },
        ..Default::default()
    };
    let link_descs: Vec<_> = std::iter::repeat_n(link_desc, 5).collect();
    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10000, 10_000_000, false, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "current_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn five_x_unlimited_current_thread() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg { speed: 0, latency: None, ..Default::default() },
        ..Default::default()
    };
    let link_descs: Vec<_> = std::iter::repeat_n(link_desc, 5).collect();
    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10000, 10_000_000, false, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn five_x_very_high_latency() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 10_000_000,
            latency: Some(Duration::from_millis(1000)),
            buffer_size: 10_000_000,
            buffer_items: 50_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let link_descs: Vec<_> = std::iter::repeat_n(link_desc, 5).collect();

    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(20_000_000).unwrap(),
        recv_buffer: NonZeroU32::new(20_000_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_ack_timeout_max: Duration::from_secs(15),
        link_non_working_timeout: Duration::from_secs(30),
        link_unacked_init: NonZeroUsize::new(10_000_000).unwrap(),
        ..Default::default()
    };

    multi_link_test(&link_descs, alc_cfg, 16384, 30000, 4_000_000, false, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn five_x_blocked() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg { speed: 0, latency: None, ..Default::default() },
        ..Default::default()
    };
    let mut link_descs: Vec<_> = std::iter::repeat_n(link_desc, 5).collect();

    link_descs[0].block = Some((0, Duration::from_secs(1)));
    link_descs[1].block = Some((1000, Duration::from_secs(1)));
    link_descs[2].block = Some((2000, Duration::from_secs(1)));
    link_descs[3].block = Some((5000, Duration::from_secs(1)));
    link_descs[4].block = Some((9990, Duration::from_secs(1)));

    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10000, 10_000_000, false, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn ten_x_hundert_kb_per_s() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 100_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 4096,
            ..Default::default()
        },
        ..Default::default()
    };
    let link_descs: Vec<_> = std::iter::repeat_n(link_desc, 10).collect();

    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 100, 500_000, false, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn ten_x_paused_link() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc { pause: Some((n * 100, Duration::from_secs(3))), ..link_desc.clone() });
    }

    let alc_cfg = Cfg { link_retest_interval: Duration::from_secs(2), ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10_000, 3_000_000, false, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn ten_x_failed_link() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc {
            pause: if n % 2 == 0 { Some((n * 100, Duration::from_secs(1))) } else { None },
            fail: if n != 9 { Some(n * 100 + 50) } else { None },
            ..link_desc.clone()
        });
    }

    let alc_cfg = Cfg {
        link_retest_interval: Duration::from_secs(2),
        no_link_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    timeout(Duration::from_secs(60), multi_link_test(&link_descs, alc_cfg, 16384, 2_000, 500_000, false, None))
        .await
        .unwrap();
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn ten_x_all_failed_link() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc {
            pause: if n % 2 == 0 { Some((n * 100, Duration::from_secs(1))) } else { None },
            fail: Some(n * 100 + 50),
            ..link_desc.clone()
        });
    }

    let alc_cfg = Cfg {
        link_retest_interval: Duration::from_secs(2),
        no_link_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    multi_link_test(&link_descs, alc_cfg, 16384, 2_000, 0, true, None).await;
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn ten_x_link_timeout() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc {
            pause: if n != 9 { Some((n * 100, Duration::from_secs(10000))) } else { None },
            fail: if n != 9 { Some(1_000_000) } else { None },
            ..link_desc.clone()
        });
    }

    let alc_cfg = Cfg {
        link_ping_timeout: Duration::from_secs(10),
        link_non_working_timeout: Duration::from_secs(5),
        link_retest_interval: Duration::from_secs(2),
        no_link_timeout: Duration::from_secs(10),
        link_ping: LinkPing::WhenIdle(Duration::from_secs(1)),
        ..Default::default()
    };

    timeout(Duration::from_secs(60), multi_link_test(&link_descs, alc_cfg, 16384, 3_000, 0, false, None))
        .await
        .unwrap();
}

#[cfg_attr(not(feature = "js"), test_log::test(tokio::test(flavor = "multi_thread")))]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn forceful_termination() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 10_000_000,
            latency: Some(Duration::from_millis(1000)),
            buffer_size: 10_000_000,
            buffer_items: 50_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let link_descs: Vec<_> = std::iter::repeat_n(link_desc, 5).collect();

    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(20_000_000).unwrap(),
        recv_buffer: NonZeroU32::new(20_000_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_ack_timeout_max: Duration::from_secs(15),
        link_non_working_timeout: Duration::from_secs(30),
        link_unacked_init: NonZeroUsize::new(10_000_000).unwrap(),
        ..Default::default()
    };

    multi_link_test(&link_descs, alc_cfg, 16384, 30000, 4_000_000, false, Some(10000)).await;
}

#[cfg(not(feature = "js"))]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn datagram_low_latency_hedges_loss() {
    let bandwidth = datagram_loss_roundtrip(AggMode::Bandwidth).await;
    let bandwidth_redundant = datagram_loss_roundtrip(AggMode::BandwidthRedundant).await;
    let low_latency = datagram_loss_roundtrip(AggMode::LowLatency).await;

    println!("bandwidth loss run: {bandwidth:?}");
    println!("bandwidth-redundant loss run: {bandwidth_redundant:?}");
    println!("low-latency loss run: {low_latency:?}");

    assert_eq!(bandwidth.stats.hedge_sent, 0, "bandwidth mode must not hedge");
    assert!(bandwidth_redundant.stats.hedge_sent > 0, "bandwidth-redundant mode did not send hedge copies");
    assert!(
        bandwidth_redundant.stats.hedge_won > 0,
        "no bandwidth-redundant hedge copy arrived before the original copy"
    );
    assert!(low_latency.stats.hedge_sent > 0, "low-latency mode did not send hedge copies");
    assert!(low_latency.stats.hedge_won > 0, "no hedge copy arrived before the original copy");
}

#[cfg(not(feature = "js"))]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "local latency simulation benchmark"]
async fn latency_sim_tcp_like_vs_datagram_low_latency() {
    let tcp_like = latency_sim_roundtrip(LatencySimMode::TcpLikeBandwidth).await;
    let datagram_bandwidth = latency_sim_roundtrip(LatencySimMode::DatagramBandwidth).await;
    let datagram_bandwidth_redundant = latency_sim_roundtrip(LatencySimMode::DatagramBandwidthRedundant).await;
    let datagram_low_latency = latency_sim_roundtrip(LatencySimMode::DatagramLowLatency).await;

    println!("\nlocal multi-link latency simulation");
    println!("scenario: two links, 8ms one-way base latency; one response link is impaired");
    println!("tcp-like: impaired response link has periodic 120ms HOL stalls");
    println!("datagram: impaired response link drops data datagrams; low-latency may hedge over the clean link");
    tcp_like.print();
    datagram_bandwidth.print();
    datagram_bandwidth_redundant.print();
    datagram_low_latency.print();

    assert_eq!(tcp_like.stats.hedge_sent, 0, "tcp-like bandwidth mode should not hedge");
    assert_eq!(datagram_bandwidth.stats.hedge_sent, 0, "datagram bandwidth mode should not hedge");
    assert!(datagram_bandwidth_redundant.stats.hedge_won > 0, "bandwidth-redundant mode did not win any hedge");
    assert!(datagram_low_latency.stats.hedge_won > 0, "low-latency mode did not win any hedge");
    assert!(datagram_low_latency.p95 < tcp_like.p95, "datagram low-latency p95 was not below tcp-like p95");
}

#[cfg(not(feature = "js"))]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "local asymmetric RTT/loss latency simulation benchmark"]
async fn latency_sim_asymmetric_rtt_and_loss() {
    let profile = AsymmetricLossProfile { fast_loss_every: 20, slow_loss_every: 10 };
    let tcp_like = asymmetric_latency_loss_roundtrip(LatencySimMode::TcpLikeBandwidth, profile).await;
    let datagram_low_latency =
        asymmetric_latency_loss_roundtrip(LatencySimMode::DatagramLowLatency, profile).await;

    println!("\nlocal asymmetric multi-link latency/loss simulation");
    println!(
        "scenario: link0 RTT ~= 100ms with ~{:.1}% loss; link1 RTT ~= 150ms with ~{:.1}% loss",
        profile.fast_loss_percent(),
        profile.slow_loss_percent()
    );
    println!("tcp-like: loss is represented as response-path HOL stalls");
    println!("datagram: loss is represented as dropped response data datagrams");
    tcp_like.print();
    datagram_low_latency.print();

    assert_eq!(tcp_like.stats.hedge_sent, 0, "tcp-like bandwidth mode should not hedge");
    assert!(datagram_low_latency.p95 < tcp_like.p95, "datagram low-latency p95 was not below tcp-like p95");
}

#[cfg(not(feature = "js"))]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "local asymmetric RTT/loss latency simulation benchmark"]
async fn latency_sim_fast_link_lossy_slow_link_cleaner() {
    let profile = AsymmetricLossProfile { fast_loss_every: 10, slow_loss_every: 50 };
    let tcp_like = asymmetric_latency_loss_roundtrip(LatencySimMode::TcpLikeBandwidth, profile).await;
    let datagram_low_latency =
        asymmetric_latency_loss_roundtrip(LatencySimMode::DatagramLowLatency, profile).await;

    println!("\nlocal asymmetric multi-link latency/loss simulation");
    println!(
        "scenario: link0 RTT ~= 100ms with ~{:.1}% loss; link1 RTT ~= 150ms with ~{:.1}% loss",
        profile.fast_loss_percent(),
        profile.slow_loss_percent()
    );
    println!("tcp-like: loss is represented as response-path HOL stalls");
    println!("datagram: loss is represented as dropped response data datagrams");
    tcp_like.print();
    datagram_low_latency.print();

    assert_eq!(tcp_like.stats.hedge_sent, 0, "tcp-like bandwidth mode should not hedge");
    assert!(datagram_low_latency.p95 < tcp_like.p95, "datagram low-latency p95 was not below tcp-like p95");
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn loss_percent(every: usize) -> f64 {
    100.0 / every as f64
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    assert!(!sorted.is_empty());
    let idx = ((sorted.len() - 1) * percentile).div_ceil(100);
    sorted[idx]
}

fn latency_run(mode: LatencySimMode, mut samples: Vec<Duration>, stats: Stats) -> LatencyRun {
    samples.sort_unstable();
    LatencyRun {
        mode,
        p50: percentile(&samples, 50),
        p95: percentile(&samples, 95),
        p99: percentile(&samples, 99),
        max: *samples.last().unwrap(),
        stats,
    }
}

async fn latency_sim_roundtrip(mode: LatencySimMode) -> LatencyRun {
    const LINK_COUNT: usize = 2;
    const ROUNDTRIPS: usize = 80;

    let link_cfg = test_channel::Cfg {
        speed: 0,
        latency: Some(Duration::from_millis(8)),
        buffer_items: 4096,
        buffer_size: 1_000_000,
        ..Default::default()
    };

    let alc_cfg = Cfg {
        agg_mode: mode.agg_mode(),
        max_redundancy_overhead_percent: 50,
        hedge_delay_min: Duration::from_millis(5),
        hedge_delay_max: Duration::from_millis(30),
        link_ack_timeout_min: Duration::from_millis(1_000),
        link_ack_timeout_roundtrip_factor: NonZeroU32::new(1).unwrap(),
        link_ack_timeout_max: Duration::from_millis(1_000),
        link_retest_interval: Duration::from_millis(50),
        link_test_data_limit: 0,
        link_ping_timeout: Duration::from_secs(10),
        link_non_working_timeout: Duration::from_secs(20),
        no_link_timeout: Duration::from_secs(20),
        link_flush_delay: Duration::from_millis(1),
        stats_intervals: vec![Duration::from_millis(10)],
        ..Default::default()
    };

    let mut server_links = Vec::new();
    let mut client_links = Vec::new();
    let mut impair_controls = Vec::new();

    for _ in 0..LINK_COUNT {
        let (client_to_server_tx, client_to_server_rx, client_to_server_control) =
            test_channel::channel(link_cfg.clone());
        let (server_to_client_tx, server_to_client_rx, server_to_client_control) =
            test_channel::channel(link_cfg.clone());

        let server_stream = if mode.is_datagram() {
            DatagramBox::new(server_to_client_tx, client_to_server_rx).into_tx_rx()
        } else {
            TxRxBox::new(server_to_client_tx, client_to_server_rx)
        };
        let client_stream = if mode.is_datagram() {
            DatagramBox::new(client_to_server_tx, server_to_client_rx).into_tx_rx()
        } else {
            TxRxBox::new(client_to_server_tx, server_to_client_rx)
        };
        server_links.push(server_stream.into_split());
        client_links.push(client_stream.into_split());

        impair_controls.push(client_to_server_control);
        impair_controls.push(server_to_client_control);
    }

    let (server_ready_tx, server_ready_rx) = oneshot::channel();
    let client_controls = impair_controls.clone();

    let server_cfg = alc_cfg.clone();
    let server_task = async move {
        let server = Server::new(server_cfg);
        let mut listener = server.listen().unwrap();

        let mut added_links = Vec::new();
        for (n, (tx, rx)) in server_links.into_iter().enumerate() {
            added_links.push(server.add_incoming(tx, rx, format!("{n}"), &[]).await.unwrap());
        }

        let incoming = listener.next().await.unwrap();
        let (task, ch, mut control) = incoming.accept();
        let task = exec::spawn(task.into_future());

        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let (tx, mut rx) = ch.into_tx_rx();
        let _ = server_ready_tx.send(());

        for _ in 0..ROUNDTRIPS {
            let data = timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap().unwrap();
            tx.send(data).await.unwrap();
            tx.flush().await.unwrap();
        }

        sleep(Duration::from_millis(30)).await;
        let stats = control.stats();
        drop(tx);
        drop(rx);

        control.terminated().await.expect("server control failed");
        task.await.unwrap().expect("server task failed");

        stats
    };

    let client_task = async move {
        let (task, outgoing, mut control) = connect(alc_cfg);
        let task = exec::spawn(task.into_future());

        let mut added_link_tasks = Vec::new();
        for (n, (tx, rx)) in client_links.into_iter().enumerate() {
            added_link_tasks.push(control.add(tx, rx, format!("{n}"), &[]));
        }
        let added_links = future::try_join_all(added_link_tasks).await.unwrap();

        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let ch = outgoing.connect().await.unwrap();
        let (tx, mut rx) = ch.into_tx_rx();

        server_ready_rx.await.unwrap();
        for (n, control) in client_controls.iter().enumerate() {
            control.set_drop_every(None, 0).await.unwrap();
            control.set_drop_data_every(None, 0).await.unwrap();
            control.set_jitter_every(None, 0, Duration::ZERO).await.unwrap();

            if n == 1 {
                match mode {
                    LatencySimMode::TcpLikeBandwidth => {
                        control.set_jitter_every(Some(5), 0, Duration::from_millis(120)).await.unwrap();
                    }
                    LatencySimMode::DatagramBandwidth
                    | LatencySimMode::DatagramBandwidthRedundant
                    | LatencySimMode::DatagramLowLatency => {
                        control.set_drop_data_every(Some(1), 0).await.unwrap();
                    }
                }
            }
        }

        let mut samples = Vec::with_capacity(ROUNDTRIPS);
        for i in 0..ROUNDTRIPS {
            let data = Bytes::from((i as u32).to_be_bytes().to_vec());
            let started = Instant::now();
            tx.send(data.clone()).await.unwrap();
            tx.flush().await.unwrap();
            let echo = timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap().unwrap();
            samples.push(started.elapsed());
            assert_eq!(echo, data);
        }

        sleep(Duration::from_millis(30)).await;
        let stats = control.stats();
        drop(tx);
        drop(rx);

        control.terminated().await.expect("client control failed");
        task.await.unwrap().expect("client task failed");

        (samples, stats)
    };

    let (server_stats, (samples, mut client_stats)) = join!(server_task, client_task);
    client_stats.primary_sent += server_stats.primary_sent;
    client_stats.redundant_sent += server_stats.redundant_sent;
    client_stats.hedge_sent += server_stats.hedge_sent;
    client_stats.hedge_won += server_stats.hedge_won;
    client_stats.duplicate_data_received += server_stats.duplicate_data_received;
    client_stats.redundancy_overhead_percent = if client_stats.primary_sent == 0 {
        0
    } else {
        client_stats.redundant_sent * 100 / client_stats.primary_sent
    };

    latency_run(mode, samples, client_stats)
}

async fn asymmetric_latency_loss_roundtrip(mode: LatencySimMode, profile: AsymmetricLossProfile) -> LatencyRun {
    const ROUNDTRIPS: usize = 60;

    let link_cfgs = [
        test_channel::Cfg {
            speed: 0,
            latency: Some(Duration::from_millis(50)),
            buffer_items: 4096,
            buffer_size: 1_000_000,
            ..Default::default()
        },
        test_channel::Cfg {
            speed: 0,
            latency: Some(Duration::from_millis(75)),
            buffer_items: 4096,
            buffer_size: 1_000_000,
            ..Default::default()
        },
    ];

    let alc_cfg = Cfg {
        agg_mode: mode.agg_mode(),
        max_redundancy_overhead_percent: 50,
        hedge_delay_min: Duration::from_millis(25),
        hedge_delay_max: Duration::from_millis(220),
        link_ack_timeout_min: Duration::from_millis(600),
        link_ack_timeout_roundtrip_factor: NonZeroU32::new(2).unwrap(),
        link_ack_timeout_max: Duration::from_millis(600),
        link_retest_interval: Duration::from_millis(100),
        link_test_data_limit: 0,
        link_ping_timeout: Duration::from_secs(10),
        link_non_working_timeout: Duration::from_secs(120),
        no_link_timeout: Duration::from_secs(120),
        link_flush_delay: Duration::from_millis(1),
        stats_intervals: vec![Duration::from_millis(20)],
        ..Default::default()
    };

    let mut server_links = Vec::new();
    let mut client_links = Vec::new();
    let mut impair_controls = Vec::new();

    for link_cfg in link_cfgs {
        let (client_to_server_tx, client_to_server_rx, client_to_server_control) =
            test_channel::channel(link_cfg.clone());
        let (server_to_client_tx, server_to_client_rx, server_to_client_control) =
            test_channel::channel(link_cfg);

        let server_stream = if mode.is_datagram() {
            DatagramBox::new(server_to_client_tx, client_to_server_rx).into_tx_rx()
        } else {
            TxRxBox::new(server_to_client_tx, client_to_server_rx)
        };
        let client_stream = if mode.is_datagram() {
            DatagramBox::new(client_to_server_tx, server_to_client_rx).into_tx_rx()
        } else {
            TxRxBox::new(client_to_server_tx, server_to_client_rx)
        };
        server_links.push(server_stream.into_split());
        client_links.push(client_stream.into_split());

        impair_controls.push(client_to_server_control);
        impair_controls.push(server_to_client_control);
    }

    let (server_ready_tx, server_ready_rx) = oneshot::channel();
    let client_controls = impair_controls.clone();

    let server_cfg = alc_cfg.clone();
    let server_task = async move {
        let server = Server::new(server_cfg);
        let mut listener = server.listen().unwrap();

        let mut added_links = Vec::new();
        for (n, (tx, rx)) in server_links.into_iter().enumerate() {
            added_links.push(server.add_incoming(tx, rx, format!("{n}"), &[]).await.unwrap());
        }

        let incoming = listener.next().await.unwrap();
        let (task, ch, mut control) = incoming.accept();
        let task = exec::spawn(task.into_future());

        timeout(Duration::from_secs(2), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let (tx, mut rx) = ch.into_tx_rx();
        let _ = server_ready_tx.send(());

        for _ in 0..ROUNDTRIPS {
            let data = timeout(Duration::from_secs(60), rx.recv()).await.unwrap().unwrap().unwrap();
            tx.send(data).await.unwrap();
            tx.flush().await.unwrap();
        }

        sleep(Duration::from_millis(50)).await;
        let stats = control.stats();
        drop(tx);
        drop(rx);

        control.terminated().await.expect("server control failed");
        task.await.unwrap().expect("server task failed");

        stats
    };

    let client_task = async move {
        let (task, outgoing, mut control) = connect(alc_cfg);
        let task = exec::spawn(task.into_future());

        let mut added_link_tasks = Vec::new();
        for (n, (tx, rx)) in client_links.into_iter().enumerate() {
            added_link_tasks.push(control.add(tx, rx, format!("{n}"), &[]));
        }
        let added_links = future::try_join_all(added_link_tasks).await.unwrap();

        timeout(Duration::from_secs(2), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let ch = outgoing.connect().await.unwrap();
        let (tx, mut rx) = ch.into_tx_rx();

        server_ready_rx.await.unwrap();
        for (n, control) in client_controls.iter().enumerate() {
            control.set_drop_every(None, 0).await.unwrap();
            control.set_drop_data_every(None, 0).await.unwrap();
            control.set_jitter_every(None, 0, Duration::ZERO).await.unwrap();

            match (mode, n) {
                (LatencySimMode::TcpLikeBandwidth, 1) => {
                    control
                        .set_jitter_every(Some(profile.fast_loss_every), 0, Duration::from_millis(180))
                        .await
                        .unwrap();
                }
                (LatencySimMode::TcpLikeBandwidth, 3) => {
                    control
                        .set_jitter_every(Some(profile.slow_loss_every), 0, Duration::from_millis(220))
                        .await
                        .unwrap();
                }
                (
                    LatencySimMode::DatagramBandwidth
                    | LatencySimMode::DatagramBandwidthRedundant
                    | LatencySimMode::DatagramLowLatency,
                    1,
                ) => {
                    control.set_drop_data_every(Some(profile.fast_loss_every), 0).await.unwrap();
                }
                (
                    LatencySimMode::DatagramBandwidth
                    | LatencySimMode::DatagramBandwidthRedundant
                    | LatencySimMode::DatagramLowLatency,
                    3,
                ) => {
                    control.set_drop_data_every(Some(profile.slow_loss_every), 0).await.unwrap();
                }
                _ => {}
            }
        }

        let mut samples = Vec::with_capacity(ROUNDTRIPS);
        for i in 0..ROUNDTRIPS {
            let data = Bytes::from((i as u32).to_be_bytes().to_vec());
            let started = Instant::now();
            tx.send(data.clone()).await.unwrap();
            tx.flush().await.unwrap();
            let echo = timeout(Duration::from_secs(60), rx.recv()).await.unwrap().unwrap().unwrap();
            samples.push(started.elapsed());
            assert_eq!(echo, data);
        }

        sleep(Duration::from_millis(50)).await;
        let stats = control.stats();
        drop(tx);
        drop(rx);

        control.terminated().await.expect("client control failed");
        task.await.unwrap().expect("client task failed");

        (samples, stats)
    };

    let (server_stats, (samples, mut client_stats)) = join!(server_task, client_task);
    client_stats.primary_sent += server_stats.primary_sent;
    client_stats.redundant_sent += server_stats.redundant_sent;
    client_stats.hedge_sent += server_stats.hedge_sent;
    client_stats.hedge_won += server_stats.hedge_won;
    client_stats.duplicate_data_received += server_stats.duplicate_data_received;
    client_stats.redundancy_overhead_percent = if client_stats.primary_sent == 0 {
        0
    } else {
        client_stats.redundant_sent * 100 / client_stats.primary_sent
    };

    latency_run(mode, samples, client_stats)
}

async fn datagram_loss_roundtrip(agg_mode: AggMode) -> LossRun {
    const LINK_COUNT: usize = 2;
    const ROUNDTRIPS: usize = 24;

    let link_cfg = test_channel::Cfg {
        speed: 0,
        latency: Some(Duration::from_millis(8)),
        buffer_items: 4096,
        buffer_size: 1_000_000,
        ..Default::default()
    };

    let alc_cfg = Cfg {
        agg_mode,
        max_redundancy_overhead_percent: 50,
        hedge_delay_min: Duration::from_millis(5),
        hedge_delay_max: Duration::from_millis(30),
        link_ack_timeout_min: Duration::from_millis(120),
        link_ack_timeout_roundtrip_factor: NonZeroU32::new(1).unwrap(),
        link_ack_timeout_max: Duration::from_millis(120),
        link_retest_interval: Duration::from_millis(20),
        link_test_data_limit: 0,
        link_ping_timeout: Duration::from_secs(10),
        link_non_working_timeout: Duration::from_secs(20),
        no_link_timeout: Duration::from_secs(20),
        link_flush_delay: Duration::from_millis(1),
        stats_intervals: vec![Duration::from_millis(5)],
        ..Default::default()
    };

    let mut server_links = Vec::new();
    let mut client_links = Vec::new();
    let mut impair_controls = Vec::new();

    for _ in 0..LINK_COUNT {
        let (client_to_server_tx, client_to_server_rx, client_to_server_control) =
            test_channel::channel(link_cfg.clone());
        let (server_to_client_tx, server_to_client_rx, server_to_client_control) =
            test_channel::channel(link_cfg.clone());

        let server_stream = DatagramBox::new(server_to_client_tx, client_to_server_rx).into_tx_rx();
        let client_stream = DatagramBox::new(client_to_server_tx, server_to_client_rx).into_tx_rx();
        server_links.push(server_stream.into_split());
        client_links.push(client_stream.into_split());

        impair_controls.push(client_to_server_control);
        impair_controls.push(server_to_client_control);
    }

    let (server_ready_tx, server_ready_rx) = oneshot::channel();
    let client_controls = impair_controls.clone();

    let server_cfg = alc_cfg.clone();
    let server_task = async move {
        let server = Server::new(server_cfg);
        let mut listener = server.listen().unwrap();

        let mut added_links = Vec::new();
        for (n, (tx, rx)) in server_links.into_iter().enumerate() {
            added_links.push(server.add_incoming(tx, rx, format!("{n}"), &[]).await.unwrap());
        }

        let incoming = listener.next().await.unwrap();
        let (task, ch, mut control) = incoming.accept();
        let task = exec::spawn(task.into_future());

        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let (tx, mut rx) = ch.into_tx_rx();
        let _ = server_ready_tx.send(());

        for _ in 0..ROUNDTRIPS {
            let data = timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap().unwrap();
            tx.send(data).await.unwrap();
            tx.flush().await.unwrap();
        }

        sleep(Duration::from_millis(20)).await;
        let stats = control.stats();
        drop(tx);
        drop(rx);

        control.terminated().await.expect("server control failed");
        task.await.unwrap().expect("server task failed");

        stats
    };

    let client_task = async move {
        let (task, outgoing, mut control) = connect(alc_cfg);
        let task = exec::spawn(task.into_future());

        let mut added_link_tasks = Vec::new();
        for (n, (tx, rx)) in client_links.into_iter().enumerate() {
            added_link_tasks.push(control.add(tx, rx, format!("{n}"), &[]));
        }
        let added_links = future::try_join_all(added_link_tasks).await.unwrap();

        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let ch = outgoing.connect().await.unwrap();
        let (tx, mut rx) = ch.into_tx_rx();

        server_ready_rx.await.unwrap();
        for (n, control) in client_controls.iter().enumerate() {
            control.set_drop_every(None, 0).await.unwrap();
            let drop_data_every = if n == 1 { Some(1) } else { None };
            control.set_drop_data_every(drop_data_every, 0).await.unwrap();
        }

        for i in 0..ROUNDTRIPS {
            let data = Bytes::from((i as u32).to_be_bytes().to_vec());
            tx.send(data.clone()).await.unwrap();
            tx.flush().await.unwrap();
            let echo = timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap().unwrap();
            assert_eq!(echo, data);
        }

        sleep(Duration::from_millis(20)).await;
        let stats = control.stats();
        drop(tx);
        drop(rx);

        control.terminated().await.expect("client control failed");
        task.await.unwrap().expect("client task failed");

        LossRun { stats }
    };

    let (server_stats, mut client_run) = join!(server_task, client_task);
    client_run.stats.primary_sent += server_stats.primary_sent;
    client_run.stats.redundant_sent += server_stats.redundant_sent;
    client_run.stats.hedge_sent += server_stats.hedge_sent;
    client_run.stats.hedge_won += server_stats.hedge_won;
    client_run.stats.duplicate_data_received += server_stats.duplicate_data_received;
    client_run.stats.redundancy_overhead_percent = if client_run.stats.primary_sent == 0 {
        0
    } else {
        client_run.stats.redundant_sent * 100 / client_run.stats.primary_sent
    };
    client_run
}

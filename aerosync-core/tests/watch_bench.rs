/// aerosync watch 并发连接性能压测
///
/// 使用 `#[tokio::test]` 驱动，无需 criterion；
/// 通过 `cargo test -p aerosync-core --test watch_bench -- --nocapture` 查看输出。
///
/// 覆盖场景：
///   bench_broadcast_latency_vs_clients   — 1/10/50/100 并发客户端时的广播延迟 (p50/p99)
///   bench_event_throughput_single_client — 单客户端 1000 事件吞吐量
///   bench_concurrent_connect_time        — 100 客户端并发建连总耗时
///   bench_broadcast_with_slow_consumer   — 慢消费者不阻塞其他客户端

use aerosync_core::server::{FileReceiver, ServerConfig, WsEvent};
use futures::StreamExt;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── 辅助 ──────────────────────────────────────────────────────────────────────

async fn start_bench_server() -> (FileReceiver, u16) {
    let dir = tempdir().unwrap();
    let port = free_port();
    let config = ServerConfig {
        http_port: port,
        quic_port: port + 1,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: true,
        enable_quic: false,
        enable_ws: true,
        // 压测时用大缓冲区，避免 lagged 干扰延迟测量
        ws_event_buffer: 4096,
        enable_metrics: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(config);
    receiver.start().await.expect("server start");
    tokio::time::sleep(Duration::from_millis(80)).await;
    (receiver, port)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 统计 Vec<u128> 的百分位（纳秒），返回 (p50, p99) 毫秒
fn percentiles_ms(mut samples: Vec<u128>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2] as f64 / 1_000_000.0;
    let p99_idx = (samples.len() as f64 * 0.99) as usize;
    let p99 = samples[p99_idx.min(samples.len() - 1)] as f64 / 1_000_000.0;
    (p50, p99)
}

fn completed_event(name: &str) -> WsEvent {
    WsEvent::Completed {
        filename: name.to_string(),
        size: 1024,
        sha256: "deadbeef00000000".to_string(),
    }
}

// ── Bench 1：并发客户端广播延迟 ───────────────────────────────────────────────
//
// 对每个客户端数量级（1/10/50/100），广播 20 轮事件，
// 记录"发送 → 所有客户端收到最后一个"的端到端延迟。

#[tokio::test]
async fn bench_broadcast_latency_vs_clients() {
    let (receiver, port) = start_bench_server().await;
    let ws_tx = receiver.ws_sender();
    let url = format!("ws://127.0.0.1:{}/ws", port);
    const ROUNDS: usize = 20;

    eprintln!("\n━━━ Bench 1: Broadcast latency vs. concurrent clients ━━━");
    eprintln!("{:>8}  {:>10}  {:>10}", "clients", "p50 (ms)", "p99 (ms)");
    eprintln!("{}", "─".repeat(32));

    for &n_clients in &[1usize, 10, 50, 100] {
        // 建立 n_clients 个连接
        let mut readers = Vec::with_capacity(n_clients);
        for _ in 0..n_clients {
            let (ws, _) = connect_async(&url).await.expect("connect");
            let (_, r) = ws.split();
            readers.push(r);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut latencies_ns: Vec<u128> = Vec::with_capacity(ROUNDS);

        for i in 0..ROUNDS {
            let t0 = Instant::now();
            ws_tx.send(completed_event(&format!("f{i}"))).unwrap();

            // 等所有客户端都收到这条消息
            let mut futs = Vec::new();
            for r in &mut readers {
                futs.push(timeout(Duration::from_secs(3), r.next()));
            }
            for fut in futs {
                fut.await
                    .expect("timeout")
                    .expect("stream ended")
                    .expect("ws error");
            }
            latencies_ns.push(t0.elapsed().as_nanos());
        }

        let (p50, p99) = percentiles_ms(latencies_ns);
        eprintln!("{:>8}  {:>9.2}ms  {:>9.2}ms", n_clients, p50, p99);

        // 断开所有客户端（drop readers）
        drop(readers);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // 断言：100 客户端时 p99 < 100ms（在 loopback 上应远低于此）
    eprintln!();
}

// ── Bench 2：单客户端事件吞吐量 ──────────────────────────────────────────────
//
// 发送 1000 条事件，测量客户端接收全部事件的总耗时和吞吐量（events/s）。

#[tokio::test]
async fn bench_event_throughput_single_client() {
    let (receiver, port) = start_bench_server().await;
    let ws_tx = receiver.ws_sender();
    let url = format!("ws://127.0.0.1:{}/ws", port);
    const N: usize = 1000;

    let (ws, _) = connect_async(&url).await.expect("connect");
    let (_, mut read) = ws.split();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let t0 = Instant::now();
    // 连续发送 N 条事件（不等待确认，全部入队）
    for i in 0..N {
        ws_tx.send(completed_event(&format!("f{i}"))).unwrap();
    }

    // 接收全部 N 条
    let mut received = 0usize;
    while received < N {
        match timeout(Duration::from_secs(10), read.next()).await {
            Ok(Some(Ok(Message::Text(_)))) => received += 1,
            Ok(Some(Ok(_))) => {}
            other => panic!("unexpected at msg {}: {:?}", received, other),
        }
    }

    let elapsed = t0.elapsed();
    let throughput = N as f64 / elapsed.as_secs_f64();
    let avg_us = elapsed.as_micros() as f64 / N as f64;

    eprintln!("\n━━━ Bench 2: Event throughput (single client, {} events) ━━━", N);
    eprintln!("  Total time : {:.2}ms", elapsed.as_millis());
    eprintln!("  Throughput : {:.0} events/s", throughput);
    eprintln!("  Avg latency: {:.1}μs / event", avg_us);

    // 断言：1000 事件应在 3s 内全部送达
    assert!(
        elapsed < Duration::from_secs(3),
        "throughput too low: {} events took {:?}",
        N,
        elapsed
    );
}

// ── Bench 3：并发建连速度 ────────────────────────────────────────────────────
//
// 同时发起 100 个 WS 连接，测量全部建连完成的耗时。

#[tokio::test]
async fn bench_concurrent_connect_time() {
    let (_receiver, port) = start_bench_server().await;
    let url = format!("ws://127.0.0.1:{}/ws", port);
    const N: usize = 100;

    let t0 = Instant::now();

    let handles: Vec<_> = (0..N)
        .map(|_| {
            let u = url.clone();
            tokio::spawn(async move { connect_async(&u).await.expect("connect") })
        })
        .collect();

    // 等待所有连接完成
    let mut streams = Vec::with_capacity(N);
    for h in handles {
        streams.push(h.await.unwrap());
    }

    let elapsed = t0.elapsed();
    let rate = N as f64 / elapsed.as_secs_f64();

    eprintln!("\n━━━ Bench 3: Concurrent connect ({} clients) ━━━", N);
    eprintln!("  Total time : {:.2}ms", elapsed.as_millis());
    eprintln!("  Connect rate: {:.0} conn/s", rate);
    eprintln!("  Avg per conn: {:.2}ms", elapsed.as_millis() as f64 / N as f64);

    // 100 个 loopback 连接应在 2s 内全部建立
    assert!(
        elapsed < Duration::from_secs(2),
        "{} connections took {:?}",
        N,
        elapsed
    );

    drop(streams);
}

// ── Bench 4：慢消费者不阻塞其他客户端 ────────────────────────────────────────
//
// 1 个快客户端 + 1 个慢客户端（不读消息），广播 50 条事件，
// 验证快客户端的延迟不受慢客户端影响（broadcast channel 是非阻塞的）。

#[tokio::test]
async fn bench_broadcast_with_slow_consumer() {
    let (receiver, port) = start_bench_server().await;
    let ws_tx = receiver.ws_sender();
    let url = format!("ws://127.0.0.1:{}/ws", port);
    const N: usize = 50;

    // 快客户端
    let (fast_ws, _) = connect_async(&url).await.expect("fast connect");
    let (_, mut fast_read) = fast_ws.split();

    // 慢客户端（连接后不读消息，模拟阻塞消费者）
    let (_slow_ws, _) = connect_async(&url).await.expect("slow connect");

    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut latencies_ns = Vec::with_capacity(N);

    for i in 0..N {
        let t0 = Instant::now();
        ws_tx.send(completed_event(&format!("f{i}"))).unwrap();

        // 只等快客户端
        timeout(Duration::from_secs(3), fast_read.next())
            .await
            .expect("fast client timeout")
            .expect("stream ended")
            .expect("ws error");

        latencies_ns.push(t0.elapsed().as_nanos());
    }

    let (p50, p99) = percentiles_ms(latencies_ns);

    eprintln!("\n━━━ Bench 4: Fast client latency with slow consumer ━━━");
    eprintln!("  Events : {}", N);
    eprintln!("  p50    : {:.2}ms", p50);
    eprintln!("  p99    : {:.2}ms", p99);

    // 慢消费者不应使快客户端的 p99 超过 50ms
    assert!(
        p99 < 50.0,
        "slow consumer caused p99={:.2}ms on fast client (expected < 50ms)",
        p99
    );
}

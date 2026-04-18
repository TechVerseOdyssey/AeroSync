//! Integration tests: aerosync watch 重连机制验证
//!
//! 每个测试场景：
//! 1. 启动真实 FileReceiver（HTTP only，随机端口）
//! 2. 用 tokio-tungstenite 客户端连接 /ws
//! 3. 广播 WsEvent，验证客户端收到
//! 4. 模拟断连 / 服务停止，验证重连行为

use aerosync::core::server::{FileReceiver, ServerConfig, WsEvent};
use futures::StreamExt;
use std::time::Duration;
use tempfile::tempdir;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── 辅助：启动只开 HTTP+WS 的 FileReceiver，返回 (receiver, port) ──────────

async fn start_receiver() -> (FileReceiver, u16) {
    let dir = tempdir().unwrap();
    // 端口 0 让 OS 自动分配——但 warp 绑定固定端口才方便测试；
    // 这里用一个随机高端口区间，循环找空闲口。
    let port = find_free_port();
    let config = ServerConfig {
        http_port: port,
        quic_port: port + 1,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: true,
        enable_quic: false,
        enable_ws: true,
        ws_event_buffer: 64,
        enable_metrics: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(config);
    receiver.start().await.expect("server start failed");
    // 短暂等待服务就绪
    tokio::time::sleep(Duration::from_millis(100)).await;
    (receiver, port)
}

/// 找一个本机空闲 TCP 端口
fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// ── 测试 1：基本 WS 连接 + 事件接收 ─────────────────────────────────────────

#[tokio::test]
async fn test_ws_receives_broadcast_event() {
    let (receiver, port) = start_receiver().await;
    let ws_tx = receiver.ws_sender();

    let url = format!("ws://127.0.0.1:{}/ws", port);
    let (ws_stream, _) = connect_async(&url).await.expect("ws connect failed");
    let (_write, mut read) = ws_stream.split();

    // 广播一个 Completed 事件
    let event = WsEvent::Completed {
        filename: "test.bin".to_string(),
        size: 1024,
        sha256: "deadbeef".to_string(),
    };
    ws_tx.send(event).unwrap();

    // 等待客户端收到消息（最多 2s）
    let msg = timeout(Duration::from_secs(2), read.next())
        .await
        .expect("timeout waiting for ws message")
        .unwrap()
        .unwrap();

    if let Message::Text(text) = msg {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["event"], "completed");
        assert_eq!(v["filename"], "test.bin");
        assert_eq!(v["size"], 1024);
    } else {
        panic!("Expected text message, got {:?}", msg);
    }
}

// ── 测试 2：多客户端同时连接，广播扇出 ───────────────────────────────────────

#[tokio::test]
async fn test_ws_broadcast_fanout_to_multiple_clients() {
    let (receiver, port) = start_receiver().await;
    let ws_tx = receiver.ws_sender();

    let url = format!("ws://127.0.0.1:{}/ws", port);

    // 建立 3 个并发 WS 连接
    let mut readers = Vec::new();
    for _ in 0..3 {
        let (ws, _) = connect_async(&url).await.expect("connect failed");
        let (_, r) = ws.split();
        readers.push(r);
    }

    // 广播一个事件
    ws_tx
        .send(WsEvent::Completed {
            filename: "fanout.bin".to_string(),
            size: 2048,
            sha256: "cafe1234".to_string(),
        })
        .unwrap();

    // 验证所有 3 个客户端都收到了消息
    for (i, mut r) in readers.into_iter().enumerate() {
        let msg = timeout(Duration::from_secs(2), r.next())
            .await
            .unwrap_or_else(|_| panic!("client {} timed out", i))
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["filename"], "fanout.bin", "client {} got wrong event", i);
        } else {
            panic!("client {} got non-text message", i);
        }
    }
}

// ── 测试 3：服务停止后新连接被拒绝 ─────────────────────────────────────────
//
// 注意：warp 停止后不主动向现有 WS 客户端发 Close frame（需等心跳超时）。
// 本测试验证：服务停止后，新的连接尝试失败，这是重连场景中最重要的路径。

#[tokio::test]
async fn test_ws_new_connection_fails_after_server_stop() {
    let (mut receiver, port) = start_receiver().await;

    // 停止服务
    receiver.stop().await.expect("stop failed");
    // 等待端口释放
    tokio::time::sleep(Duration::from_millis(200)).await;

    let url = format!("ws://127.0.0.1:{}/ws", port);
    let result = connect_async(&url).await;

    // 服务已停止，新连接应失败
    assert!(
        result.is_err(),
        "expected connection to fail after server stop"
    );
}

// ── 测试 4：重连后继续接收事件（模拟重连循环核心逻辑）────────────────────────

#[tokio::test]
async fn test_ws_reconnect_receives_event_after_restart() {
    let dir1 = tempdir().unwrap();
    let port = find_free_port();

    // 启动第一个服务
    let config = ServerConfig {
        http_port: port,
        quic_port: port + 1,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir1.path().to_path_buf(),
        enable_http: true,
        enable_quic: false,
        enable_ws: true,
        ws_event_buffer: 64,
        enable_metrics: false,
        ..ServerConfig::default()
    };
    let mut receiver1 = FileReceiver::new(config.clone());
    receiver1.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = format!("ws://127.0.0.1:{}/ws", port);

    // 第一次连接
    let (ws, _) = connect_async(&url).await.expect("first connect failed");
    let (_, mut read) = ws.split();

    // 广播第一个事件
    receiver1
        .ws_sender()
        .send(WsEvent::Completed {
            filename: "first.bin".to_string(),
            size: 100,
            sha256: "aaa".to_string(),
        })
        .unwrap();

    let msg = timeout(Duration::from_secs(2), read.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(msg, Message::Text(_)), "first event received");

    // 停止第一个服务（模拟服务重启）
    receiver1.stop().await.unwrap();
    // 等待 OS 释放端口
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 启动第二个服务（相同端口，模拟重启）
    let dir2 = tempdir().unwrap();
    let config2 = ServerConfig {
        receive_directory: dir2.path().to_path_buf(),
        ..config
    };
    let mut receiver2 = FileReceiver::new(config2);
    receiver2.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 重连
    let (ws2, _) = connect_async(&url).await.expect("reconnect failed");
    let (_, mut read2) = ws2.split();

    // 广播第二个事件
    receiver2
        .ws_sender()
        .send(WsEvent::Completed {
            filename: "second.bin".to_string(),
            size: 200,
            sha256: "bbb".to_string(),
        })
        .unwrap();

    let msg2 = timeout(Duration::from_secs(2), read2.next())
        .await
        .expect("timeout on reconnect receive")
        .unwrap()
        .unwrap();

    if let Message::Text(text) = msg2 {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["filename"], "second.bin");
    } else {
        panic!("expected text message after reconnect");
    }

    receiver2.stop().await.unwrap();
}

// ── 测试 5：事件类型完整性 ─────────────────────────────────────────────────

#[tokio::test]
async fn test_ws_all_event_types_serialized() {
    let (receiver, port) = start_receiver().await;
    let ws_tx = receiver.ws_sender();

    let url = format!("ws://127.0.0.1:{}/ws", port);
    let (ws, _) = connect_async(&url).await.unwrap();
    let (_, mut read) = ws.split();

    let events = vec![
        WsEvent::TransferStarted {
            filename: "a.bin".to_string(),
            size: 512,
            sender_ip: "1.2.3.4".to_string(),
        },
        WsEvent::Progress {
            filename: "a.bin".to_string(),
            bytes: 256,
            total: 512,
        },
        WsEvent::Completed {
            filename: "a.bin".to_string(),
            size: 512,
            sha256: "abc".to_string(),
        },
        WsEvent::Failed {
            filename: "b.bin".to_string(),
            reason: "disk full".to_string(),
        },
    ];

    let expected_event_types = ["transfer_started", "progress", "completed", "failed"];

    for event in events {
        ws_tx.send(event).unwrap();
    }

    // 收 4 条消息
    for expected_type in &expected_event_types {
        let msg = timeout(Duration::from_secs(2), read.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(text) = msg {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                v["event"].as_str().unwrap(),
                *expected_type,
                "expected event type '{}', got {:?}",
                expected_type,
                v["event"]
            );
        }
    }
}

// ── 测试 6：WS 未启用时连接被拒绝（返回非 101） ───────────────────────────

#[tokio::test]
async fn test_ws_disabled_rejects_connection() {
    let dir = tempdir().unwrap();
    let port = find_free_port();
    let config = ServerConfig {
        http_port: port,
        quic_port: port + 1,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: true,
        enable_quic: false,
        enable_ws: false, // ← 关键：禁用 WS
        ws_event_buffer: 64,
        enable_metrics: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(config);
    receiver.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = format!("ws://127.0.0.1:{}/ws", port);
    let result = connect_async(&url).await;

    // 连接应失败（服务端返回 404，不做 WS 升级）
    assert!(
        result.is_err(),
        "expected connection refused when ws disabled"
    );
}

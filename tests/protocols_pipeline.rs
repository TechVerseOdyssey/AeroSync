//! 流水线端到端集成测试
//!
//! 验证 AutoAdapter + TransferEngine 在并发上传 100 个小文件时的完整传输链路：
//! - MultiProgress-style 并发（最多 16 个文件同时传输）
//! - 每个文件独立校验成功
//! - 服务端 warp 接收并记录所有文件

use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync::protocols::{http::HttpConfig, quic::QuicConfig, AutoAdapter};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::Mutex;
use warp::Filter;

/// 启动一个 warp 测试服务器，接受 POST /upload 的 multipart 请求。
/// 返回 (addr, received_names Arc) 用于后续断言。
async fn start_test_server() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = Arc::clone(&received);

    let route = warp::post()
        .and(warp::path("upload"))
        .and(warp::multipart::form().max_length(10 * 1024 * 1024))
        .and_then(move |mut form: warp::multipart::FormData| {
            let recv = Arc::clone(&received_clone);
            async move {
                use futures::TryStreamExt;
                while let Some(part) = form.try_next().await.unwrap_or(None) {
                    let name = part.filename().unwrap_or("unknown").to_string();
                    recv.lock().await.push(name);
                }
                Ok::<_, Infallible>(warp::reply::with_status("ok", warp::http::StatusCode::OK))
            }
        });

    let (addr, server) =
        warp::serve(route).bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
    tokio::spawn(server);

    (addr, received)
}

/// 100 个小文件（各 4KB）并发上传端到端测试
#[tokio::test]
async fn test_pipeline_100_small_files() {
    let (addr, received_names) = start_test_server().await;
    let base_url = format!("http://{}/upload", addr);

    let dir = tempdir().unwrap();

    // 创建 100 个测试文件
    const FILE_COUNT: usize = 100;
    const FILE_SIZE: usize = 4 * 1024; // 4KB
    let mut expected_names: HashSet<String> = HashSet::new();

    for i in 0..FILE_COUNT {
        let name = format!("small_{:04}.bin", i);
        let path = dir.path().join(&name);
        let data = vec![(i % 256) as u8; FILE_SIZE];
        tokio::fs::write(&path, &data).await.unwrap();
        expected_names.insert(name);
    }

    // 配置：小文件高并发（16 并发）
    let config = TransferConfig {
        max_concurrent_transfers: 16,
        small_file_concurrency: 16,
        enable_resume: false,
        ..TransferConfig::default()
    };

    let http_config = HttpConfig {
        timeout_seconds: 30,
        max_retries: 2,
        ..HttpConfig::default()
    };
    let adapter = Arc::new(AutoAdapter::new(http_config, QuicConfig::default()));
    let engine = TransferEngine::new(config);
    engine.start(adapter).await.unwrap();

    // 加入所有任务
    for i in 0..FILE_COUNT {
        let name = format!("small_{:04}.bin", i);
        let path = dir.path().join(&name);
        let task = TransferTask::new_upload(path, base_url.clone(), FILE_SIZE as u64);
        engine.add_transfer(task).await.unwrap();
    }

    // 等待所有任务完成（超时 60s）
    let monitor = engine.get_progress_monitor().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    loop {
        let done = {
            let m = monitor.read().await;
            let stats = m.get_stats();
            stats.completed_files + stats.failed_files >= stats.total_files
                && stats.total_files == FILE_COUNT
        };
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timeout: not all files transferred within 60s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 验证所有任务成功，无失败
    let m = monitor.read().await;
    let stats = m.get_stats();
    assert_eq!(
        stats.total_files, FILE_COUNT,
        "Expected {} total tasks, got {}",
        FILE_COUNT, stats.total_files
    );
    assert_eq!(
        stats.failed_files, 0,
        "Expected 0 failures, got {}",
        stats.failed_files
    );
    assert_eq!(
        stats.completed_files, FILE_COUNT,
        "Expected {} completed, got {}",
        FILE_COUNT, stats.completed_files
    );

    // 验证服务端收到了所有 100 个文件
    let received = received_names.lock().await;
    let received_set: HashSet<String> = received.iter().cloned().collect();
    assert_eq!(
        received_set.len(),
        FILE_COUNT,
        "Server received {} unique files, expected {}",
        received_set.len(),
        FILE_COUNT
    );
    for name in &expected_names {
        assert!(
            received_set.contains(name),
            "Server did not receive file: {}",
            name
        );
    }
}

/// 混合文件大小测试：10 个大文件（256KB）+ 90 个小文件（1KB）
#[tokio::test]
async fn test_pipeline_mixed_file_sizes() {
    let (addr, received_names) = start_test_server().await;
    let base_url = format!("http://{}/upload", addr);

    let dir = tempdir().unwrap();

    const LARGE_COUNT: usize = 10;
    const SMALL_COUNT: usize = 90;
    const TOTAL: usize = LARGE_COUNT + SMALL_COUNT;

    // 创建大文件（256KB）
    for i in 0..LARGE_COUNT {
        let path = dir.path().join(format!("large_{:02}.bin", i));
        tokio::fs::write(&path, vec![0xABu8; 256 * 1024])
            .await
            .unwrap();
    }
    // 创建小文件（1KB）
    for i in 0..SMALL_COUNT {
        let path = dir.path().join(format!("small_{:02}.bin", i));
        tokio::fs::write(&path, vec![0xCDu8; 1024]).await.unwrap();
    }

    let config = TransferConfig {
        max_concurrent_transfers: 8,
        small_file_concurrency: 16,
        medium_file_concurrency: 8,
        enable_resume: false,
        ..TransferConfig::default()
    };
    let adapter = Arc::new(AutoAdapter::new(
        HttpConfig::default(),
        QuicConfig::default(),
    ));
    let engine = TransferEngine::new(config);
    engine.start(adapter).await.unwrap();

    for i in 0..LARGE_COUNT {
        let path = dir.path().join(format!("large_{:02}.bin", i));
        let task = TransferTask::new_upload(path, base_url.clone(), 256 * 1024);
        engine.add_transfer(task).await.unwrap();
    }
    for i in 0..SMALL_COUNT {
        let path = dir.path().join(format!("small_{:02}.bin", i));
        let task = TransferTask::new_upload(path, base_url.clone(), 1024);
        engine.add_transfer(task).await.unwrap();
    }

    let monitor = engine.get_progress_monitor().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    loop {
        let done = {
            let m = monitor.read().await;
            let stats = m.get_stats();
            stats.completed_files + stats.failed_files >= stats.total_files
                && stats.total_files == TOTAL
        };
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timeout on mixed file test"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let m = monitor.read().await;
    let stats = m.get_stats();
    assert_eq!(
        stats.failed_files, 0,
        "Unexpected failures: {}",
        stats.failed_files
    );
    assert_eq!(stats.completed_files, TOTAL);

    // 服务端收到 TOTAL 个文件
    let received = received_names.lock().await;
    assert_eq!(
        received.len(),
        TOTAL,
        "Server received {}, expected {}",
        received.len(),
        TOTAL
    );
}

/// 验证 AutoAdapter 对连接失败返回 Network 错误（非 panic）
#[tokio::test]
async fn test_auto_adapter_connection_refused_returns_network_error() {
    use aerosync::core::transfer::{ProtocolAdapter, TransferTask};
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let file = dir.path().join("test.bin");
    tokio::fs::write(&file, b"data").await.unwrap();

    let task = TransferTask {
        id: uuid::Uuid::new_v4(),
        source_path: file,
        destination: "http://127.0.0.1:19990/upload".to_string(),
        is_upload: true,
        file_size: 4,
        sha256: None,
    };
    let adapter = Arc::new(AutoAdapter::new(
        HttpConfig::default(),
        QuicConfig::default(),
    ));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = adapter.upload(&task, tx).await;
    assert!(result.is_err(), "should fail without server");
    match result {
        Err(aerosync::core::AeroSyncError::Network(_)) => {}
        Err(e) => panic!("Wrong error type: {:?}", e),
        Ok(_) => panic!("Should not succeed"),
    }
}

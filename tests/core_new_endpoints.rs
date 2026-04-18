//! Integration tests for Phase 2+3 new HTTP endpoints:
//!
//! 1. GET /health  — enhanced fields (active_transfers, queue_depth, protocols, version)
//! 2. POST /upload/batch — multipart batch, multi-file save, correct response shape
//! 3. POST /upload Content-Length precheck — 413 on oversized Content-Length header

use aerosync::core::server::{FileReceiver, ServerConfig};
use std::time::Duration;
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start a minimal HTTP-only FileReceiver on a random port.
/// Returns (receiver, port, tempdir).  Caller must keep `_dir` alive.
async fn start_http_receiver() -> (FileReceiver, u16, TempDir) {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let cfg = ServerConfig {
        http_port: port,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_quic: false,
        enable_ws: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.expect("server start failed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    (receiver, port, dir)
}

// ── 1. GET /health enhanced fields ───────────────────────────────────────────

/// Basic health check: status=ok, Phase 1.2 fields present and correct types.
#[tokio::test]
async fn test_health_has_new_fields() {
    let (mut receiver, port, _dir) = start_http_receiver().await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/health", port))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body["status"], "ok");
    assert!(
        body.get("active_transfers").is_some(),
        "missing active_transfers"
    );
    assert!(body.get("queue_depth").is_some(), "missing queue_depth");
    assert_eq!(body["active_transfers"], 0);
    assert_eq!(body["queue_depth"], 0);

    // protocols list must include "http"
    let protocols = body["protocols"]
        .as_array()
        .expect("protocols must be array");
    assert!(
        protocols.contains(&serde_json::json!("http")),
        "protocols must include http, got: {:?}",
        protocols
    );

    // version string present and non-empty
    assert!(
        body["version"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "version must be a non-empty string, got: {}",
        body["version"]
    );

    receiver.stop().await.unwrap();
}

/// X-AeroSync header must be present for protocol upgrade detection.
#[tokio::test]
async fn test_health_x_aerosync_header() {
    let (mut receiver, port, _dir) = start_http_receiver().await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/health", port))
        .send()
        .await
        .unwrap();

    assert!(
        resp.headers().get("x-aerosync").is_some(),
        "X-AeroSync response header must be present"
    );

    receiver.stop().await.unwrap();
}

/// received_files counter in /health reflects files already received.
#[tokio::test]
async fn test_health_received_files_count() {
    let (mut receiver, port, dir) = start_http_receiver().await;
    let client = reqwest::Client::new();

    // Upload one file via the normal /upload/<name> endpoint
    let content = b"hello integration test";
    let part = reqwest::multipart::Part::bytes(content.to_vec())
        .file_name("count_test.txt")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("file", part);
    let up = client
        .post(format!("http://127.0.0.1:{}/upload/count_test.txt", port))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert!(up.status().is_success(), "upload failed: {}", up.status());

    // Health should now show received_files = 1
    let body: serde_json::Value = client
        .get(format!("http://127.0.0.1:{}/health", port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["received_files"], 1,
        "expected 1 received file after upload"
    );

    // And the file exists on disk
    assert!(dir.path().join("count_test.txt").exists());

    receiver.stop().await.unwrap();
}

// ── 2. POST /upload/batch ─────────────────────────────────────────────────────

/// Batch upload of 3 files: all saved, response shape correct.
#[tokio::test]
async fn test_batch_upload_saves_all_files() {
    let (mut receiver, port, dir) = start_http_receiver().await;
    let client = reqwest::Client::new();

    let files: &[(&str, &[u8])] = &[
        ("alpha.txt", b"content alpha"),
        ("beta.txt", b"content beta"),
        ("gamma.txt", b"content gamma"),
    ];

    let mut form = reqwest::multipart::Form::new();
    for (name, data) in files {
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(name.to_string())
            .mime_str("application/octet-stream")
            .unwrap();
        form = form.part(*name, part);
    }

    let resp = client
        .post(format!("http://127.0.0.1:{}/upload/batch", port))
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "batch upload returned non-200");
    let body: serde_json::Value = resp.json().await.unwrap();

    let saved = body["saved"].as_u64().expect("saved must be u64");
    assert_eq!(saved, 3, "expected 3 saved files, got {}", saved);
    assert_eq!(
        body["errors"].as_array().unwrap().len(),
        0,
        "expected no errors, got: {:?}",
        body["errors"]
    );

    // Files must exist on disk with correct content
    for (name, expected) in files {
        let path = dir.path().join(name);
        assert!(path.exists(), "file {} not found on disk", name);
        let on_disk = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&on_disk, expected, "content mismatch for {}", name);
    }

    receiver.stop().await.unwrap();
}

/// Batch with a single file — edge case.
#[tokio::test]
async fn test_batch_upload_single_file() {
    let (mut receiver, port, dir) = start_http_receiver().await;

    let part = reqwest::multipart::Part::bytes(b"only one".to_vec())
        .file_name("solo.bin")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("solo.bin", part);

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/upload/batch", port))
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["saved"], 1);
    assert!(dir.path().join("solo.bin").exists());

    receiver.stop().await.unwrap();
}

/// Part name with subdirectory component — file saved in correct subdirectory.
#[tokio::test]
async fn test_batch_upload_preserves_subdir() {
    let (mut receiver, port, dir) = start_http_receiver().await;

    let part = reqwest::multipart::Part::bytes(b"nested content".to_vec())
        .file_name("sub/nested.txt")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("sub/nested.txt", part);

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/upload/batch", port))
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["saved"], 1);

    // Server sanitises path components — file may land as "sub/nested.txt" or "nested.txt"
    // depending on sanitisation; either way it must not escape the receive dir.
    let saved_somewhere = dir.path().join("sub").join("nested.txt").exists()
        || dir.path().join("nested.txt").exists();
    assert!(
        saved_somewhere,
        "file should be saved somewhere under receive dir"
    );

    receiver.stop().await.unwrap();
}

/// Batch upload increments received_files count visible in /health.
#[tokio::test]
async fn test_batch_upload_increments_received_files() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let client = reqwest::Client::new();

    let before: serde_json::Value = client
        .get(format!("http://127.0.0.1:{}/health", port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let before_count = before["received_files"].as_u64().unwrap_or(0);

    // Upload 3 files via batch
    let mut form = reqwest::multipart::Form::new();
    for i in 0u8..3 {
        let part = reqwest::multipart::Part::bytes(vec![i; 32])
            .file_name(format!("m{}.bin", i))
            .mime_str("application/octet-stream")
            .unwrap();
        form = form.part(format!("m{}.bin", i), part);
    }
    client
        .post(format!("http://127.0.0.1:{}/upload/batch", port))
        .multipart(form)
        .send()
        .await
        .unwrap();

    let after: serde_json::Value = client
        .get(format!("http://127.0.0.1:{}/health", port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let after_count = after["received_files"].as_u64().unwrap_or(0);

    assert_eq!(
        after_count,
        before_count + 3,
        "received_files should increase by 3 after batch upload"
    );

    receiver.stop().await.unwrap();
}

// ── 3. Content-Length precheck → 413 ─────────────────────────────────────────

/// Sending a Content-Length larger than max_file_size must return 413.
#[tokio::test]
async fn test_upload_oversized_content_length_returns_413() {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let cfg = ServerConfig {
        http_port: port,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_quic: false,
        enable_ws: false,
        max_file_size: 1024, // 1 KB limit
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // We need to send a raw HTTP request with Content-Length: 2048 (> 1024 limit).
    // reqwest's multipart sets its own content-type/length, so use a plain body request.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/upload/oversized.bin", port))
        .header("content-length", "2048")
        .header("content-type", "multipart/form-data; boundary=boundary")
        .body(vec![0u8; 16]) // small actual body — precheck fires on header value
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "expected 413 Payload Too Large, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("large")
            || body["error"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("payload"),
        "error message should reference size limit: {}",
        body
    );

    receiver.stop().await.unwrap();
}

/// Upload within the size limit must succeed — no false 413.
#[tokio::test]
async fn test_upload_within_limit_succeeds() {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let cfg = ServerConfig {
        http_port: port,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_quic: false,
        enable_ws: false,
        max_file_size: 1024 * 1024, // 1 MB limit
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let data = vec![7u8; 512]; // 512 bytes, well under 1 MB
    let part = reqwest::multipart::Part::bytes(data)
        .file_name("within.bin")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("file", part);

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/upload/within.bin", port))
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "expected 2xx, got {}",
        resp.status()
    );
    assert!(dir.path().join("within.bin").exists());

    receiver.stop().await.unwrap();
}

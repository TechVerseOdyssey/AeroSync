use crate::traits::{TransferProtocol, TransferProgress};
use crate::ratelimit::RateLimiter;
use aerosync_core::{AeroSyncError, Result, TransferTask};
use aerosync_core::resume::ResumeState;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::io::ReaderStream;

pub struct HttpTransfer {
    client: Arc<Client>,
    config: HttpConfig,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub chunk_size: usize,
    /// 发送方认证 Token（Bearer）
    pub auth_token: Option<String>,
    /// 上传带宽限制（bytes/s），0 = 不限速
    pub upload_limit_bps: u64,
    /// 是否接受无效的 TLS 证书（用于连接自签名 HTTPS 服务端，默认 false）
    pub accept_invalid_certs: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 30,
            max_retries: 3,
            chunk_size: 4 * 1024 * 1024, // 4MB
            auth_token: None,
            upload_limit_bps: 0,
            accept_invalid_certs: false,
        }
    }
}

impl HttpTransfer {
    pub fn new(config: HttpConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .danger_accept_invalid_certs(config.accept_invalid_certs)
            .build()
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        Ok(Self { client: Arc::new(client), config })
    }

    /// 使用外部共享 client 构造，避免每次创建新连接池。
    pub fn new_with_client(client: Arc<Client>, config: HttpConfig) -> Self {
        Self { client, config }
    }

    /// 分片上传：将文件切分为 chunk_size 大小的块逐一上传。
    /// 服务端路径：POST /upload/chunk?task_id=&chunk_index=&total_chunks=&filename=
    /// 所有分片完成后发送 POST /upload/complete?... 合并。
    ///
    /// `base_url`: 形如 `http://host:port`（不含路径）
    /// `state`:    ResumeState（含已完成分片，支持断点续传）
    pub async fn upload_chunked(
        &self,
        file_path: &std::path::Path,
        base_url: &str,
        state: &mut ResumeState,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let start_time = Instant::now();
        let mut bytes_done = state.bytes_transferred();
        let rate_limiter = RateLimiter::new(self.config.upload_limit_bps);

        tracing::info!(
            "Chunked upload: '{}' {} chunks (already done: {:?})",
            file_name,
            state.total_chunks,
            state.completed_chunks
        );

        for chunk_index in state.pending_chunks() {
            let offset = state.chunk_offset(chunk_index);
            let size = state.chunk_size_of(chunk_index);

            // 读取分片数据
            let mut file = File::open(file_path).await?;
            tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; size as usize];
            file.read_exact(&mut buf).await?;

            // 限速（消耗令牌）
            rate_limiter.consume(size).await;

            // 带重试的分片上传
            let chunk_url = format!(
                "{}/upload/chunk?task_id={}&chunk_index={}&total_chunks={}&filename={}",
                base_url,
                state.task_id,
                chunk_index,
                state.total_chunks,
                urlencoding::encode(&file_name)
            );

            let mut last_err = None;
            for attempt in 0..=self.config.max_retries {
                let mut req = self.client.post(&chunk_url).body(buf.clone());
                if let Some(token) = &self.config.auth_token {
                    req = req.header("Authorization", format!("Bearer {}", token));
                }
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        last_err = None;
                        break;
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        last_err = Some(AeroSyncError::Network(format!(
                            "Chunk {} failed: {} - {}",
                            chunk_index, status, body
                        )));
                        tracing::warn!("Chunk {} attempt {}/{} failed: {}", chunk_index, attempt + 1, self.config.max_retries + 1, status);
                    }
                    Err(e) => {
                        last_err = Some(AeroSyncError::Network(e.to_string()));
                        tracing::warn!("Chunk {} attempt {}/{} error: {}", chunk_index, attempt + 1, self.config.max_retries + 1, e);
                    }
                }
                if attempt < self.config.max_retries {
                    tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                }
            }
            if let Some(e) = last_err {
                return Err(e);
            }

            // 标记完成，更新进度
            state.mark_chunk_done(chunk_index);
            bytes_done += size;
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { bytes_done as f64 / elapsed } else { 0.0 };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred: bytes_done,
                transfer_speed: speed,
            });
        }

        // 所有分片完成，发送合并请求
        let mut complete_url = format!(
            "{}/upload/complete?task_id={}&filename={}&total_chunks={}",
            base_url,
            state.task_id,
            urlencoding::encode(&file_name),
            state.total_chunks,
        );
        if let Some(ref sha) = state.sha256 {
            complete_url.push_str(&format!("&sha256={}", sha));
        }

        let mut req = self.client.post(&complete_url);
        if let Some(token) = &self.config.auth_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("Complete request failed: {}", e)))?;

        if resp.status().is_success() {
            tracing::info!("Chunked upload complete: {} ({} bytes)", file_name, state.total_size);
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(AeroSyncError::Network(format!(
                "Complete failed: {} - {}",
                status, body
            )))
        }
    }

    async fn upload_with_progress(
        &self,
        file_path: &std::path::Path,
        url: &str,
        sha256: Option<&str>,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let file = File::open(file_path).await?;
        let file_size = file.metadata().await?.len();
        let start_time = Instant::now();

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        // 流式读取文件，避免整文件读入内存
        let stream = ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);

        let file_part = reqwest::multipart::Part::stream_with_length(body, file_size)
            .file_name(file_name.clone())
            .mime_str("application/octet-stream")
            .map_err(|e| AeroSyncError::Network(format!("Failed to create multipart part: {}", e)))?;

        let form = reqwest::multipart::Form::new().part("file", file_part);

        tracing::info!("HTTP: Uploading '{}' ({} bytes) to {}", file_name, file_size, url);

        let mut request = self.client.post(url).multipart(form);

        // 附加认证 Token
        if let Some(token) = &self.config.auth_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        // 附加 SHA-256 校验哈希
        if let Some(hash) = sha256 {
            request = request.header("X-File-Hash", hash);
            request = request.header("X-Hash-Algorithm", "sha256");
        }

        let response = request
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("Upload request failed: {}", e)))?;

        if response.status().is_success() {
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { file_size as f64 / elapsed } else { 0.0 };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred: file_size,
                transfer_speed: speed,
            });
            tracing::info!(
                "HTTP: Upload completed: {} bytes at {:.2} MB/s",
                file_size,
                speed / (1024.0 * 1024.0)
            );
            Ok(())
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            Err(AeroSyncError::Network(format!(
                "Upload failed with status: {} - {}",
                status, error_text
            )))
        }
    }

    async fn download_with_progress(
        &self,
        url: &str,
        file_path: &std::path::Path,
        expected_sha256: Option<&str>,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        use futures::StreamExt;

        let mut request = self.client.get(url);
        if let Some(token) = &self.config.auth_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(AeroSyncError::Network(format!(
                "Download failed with status: {}",
                response.status()
            )));
        }

        // 从响应头读取服务端提供的 SHA-256
        let server_hash = response
            .headers()
            .get("X-File-Hash")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let total_size = response.content_length().unwrap_or(0);
        let mut file = File::create(file_path).await?;
        let mut bytes_transferred = 0u64;
        let start_time = Instant::now();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AeroSyncError::Network(e.to_string()))?;
            file.write_all(&chunk).await?;

            bytes_transferred += chunk.len() as u64;
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { bytes_transferred as f64 / elapsed } else { 0.0 };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred,
                transfer_speed: speed,
            });
        }
        file.flush().await?;

        // 完成后校验 SHA-256（优先用调用方期望的 hash，其次用服务端提供的）
        let hash_to_check = expected_sha256.map(|s| s.to_string()).or(server_hash);
        if let Some(expected) = hash_to_check {
            use sha2::{Sha256, Digest};
            let data = tokio::fs::read(file_path).await?;
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let actual = hex::encode(hasher.finalize());
            if actual != expected {
                return Err(AeroSyncError::Protocol(format!(
                    "SHA-256 mismatch: expected {}, got {}",
                    expected, actual
                )));
            }
            tracing::info!("HTTP: SHA-256 verified OK");
        }

        tracing::info!(
            "HTTP: Download completed: {} / {} bytes",
            bytes_transferred,
            total_size
        );
        Ok(())
    }
}

#[async_trait]
impl TransferProtocol for HttpTransfer {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        self.upload_with_progress(&task.source_path, &task.destination, None, progress_tx)
            .await
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        self.download_with_progress(&task.destination, &task.source_path, None, progress_tx)
            .await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        if task.is_upload {
            // Phase 2: 分片续传
            let base_url = extract_base_url(&task.destination);
            let chunk_size = self.config.chunk_size as u64;

            // 根据 offset 推算已完成的分片序号
            let completed_chunks: Vec<u32> = if chunk_size > 0 {
                (0..((offset / chunk_size) as u32)).collect()
            } else {
                vec![]
            };

            let mut state = aerosync_core::resume::ResumeState::new(
                task.id,
                task.source_path.clone(),
                task.destination.clone(),
                task.file_size,
                chunk_size,
                task.sha256.clone(),
            );
            state.completed_chunks = completed_chunks;

            self.upload_chunked(&task.source_path, &base_url, &mut state, progress_tx)
                .await
        } else {
            // HTTP Range 请求续传
            use futures::StreamExt;
            let mut request = self.client
                .get(&task.destination)
                .header("Range", format!("bytes={}-", offset));
            if let Some(token) = &self.config.auth_token {
                request = request.header("Authorization", format!("Bearer {}", token));
            }
            let response = request
                .send()
                .await
                .map_err(|e| AeroSyncError::Network(e.to_string()))?;

            // 206 Partial Content 或 200 OK 均接受
            if !response.status().is_success() && response.status().as_u16() != 206 {
                return Err(AeroSyncError::Network(format!(
                    "Resume download failed: {}",
                    response.status()
                )));
            }

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&task.source_path)
                .await?;

            let start_time = Instant::now();
            let mut bytes_transferred = offset;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| AeroSyncError::Network(e.to_string()))?;
                file.write_all(&chunk).await?;
                bytes_transferred += chunk.len() as u64;
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { bytes_transferred as f64 / elapsed } else { 0.0 };
                let _ = progress_tx.send(TransferProgress {
                    bytes_transferred,
                    transfer_speed: speed,
                });
            }
            file.flush().await?;
            Ok(())
        }
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn protocol_name(&self) -> &'static str {
        "HTTP"
    }
}

/// 从完整 URL 提取 base_url（scheme + host + port，不含路径）
fn extract_base_url(url: &str) -> String {
    // 找到第三个 '/' 前的部分，即 "scheme://host:port"
    let after_scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
    let path_start = url[after_scheme..]
        .find('/')
        .map(|i| i + after_scheme)
        .unwrap_or(url.len());
    url[..path_start].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use warp::Filter;

    // ── 1. HttpConfig defaults ────────────────────────────────────────────────
    #[test]
    fn test_http_config_defaults() {
        let cfg = HttpConfig::default();
        assert_eq!(cfg.timeout_seconds, 30);
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.chunk_size, 4 * 1024 * 1024);
        assert!(cfg.auth_token.is_none());
    }

    // ── 2. HttpTransfer construction ─────────────────────────────────────────
    #[test]
    fn test_http_transfer_new_ok() {
        let result = HttpTransfer::new(HttpConfig::default());
        assert!(result.is_ok());
    }

    // ── 3. Upload to a real warp server (streaming) ───────────────────────────
    #[tokio::test]
    async fn test_http_upload_streaming() {
        // Minimal warp endpoint that accepts multipart and returns 200
        let route = warp::post()
            .and(warp::path("upload"))
            .and(warp::multipart::form().max_length(100 * 1024 * 1024))
            .and_then(|mut form: warp::multipart::FormData| async move {
                use futures::TryStreamExt;
                while let Some(_part) = form.try_next().await.unwrap_or(None) {}
                Ok::<_, Infallible>(warp::reply::with_status("ok", warp::http::StatusCode::OK))
            });

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );

        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("upload_test.bin");
        tokio::fs::write(&file_path, b"hello streaming world").await.unwrap();

        let url = format!("http://{}/upload", addr);
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .upload_with_progress(&file_path, &url, None, tx)
            .await;
        assert!(result.is_ok(), "upload should succeed: {:?}", result);
    }

    // ── 4. Upload attaches Authorization header ───────────────────────────────
    #[tokio::test]
    async fn test_http_upload_attaches_auth_header() {
        let route = warp::post()
            .and(warp::path("upload"))
            .and(warp::header::<String>("authorization"))
            .and(warp::multipart::form().max_length(1024 * 1024))
            .and_then(|auth: String, mut form: warp::multipart::FormData| async move {
                use futures::TryStreamExt;
                while let Some(_) = form.try_next().await.unwrap_or(None) {}
                if auth == "Bearer test-token" {
                    Ok::<_, Infallible>(warp::reply::with_status(
                        "ok",
                        warp::http::StatusCode::OK,
                    ))
                } else {
                    Ok(warp::reply::with_status(
                        "unauthorized",
                        warp::http::StatusCode::UNAUTHORIZED,
                    ))
                }
            });

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("auth_test.txt");
        tokio::fs::write(&file_path, b"auth data").await.unwrap();

        let cfg = HttpConfig {
            auth_token: Some("test-token".to_string()),
            ..Default::default()
        };
        let ht = HttpTransfer::new(cfg).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .upload_with_progress(&file_path, &format!("http://{}/upload", addr), None, tx)
            .await;
        assert!(result.is_ok(), "upload with auth token should succeed: {:?}", result);
    }

    // ── 5. Upload attaches X-File-Hash header ────────────────────────────────
    #[tokio::test]
    async fn test_http_upload_attaches_sha256_header() {
        let route = warp::post()
            .and(warp::path("upload"))
            .and(warp::header::optional::<String>("x-file-hash"))
            .and(warp::multipart::form().max_length(1024 * 1024))
            .and_then(
                |hash: Option<String>, mut form: warp::multipart::FormData| async move {
                    use futures::TryStreamExt;
                    while let Some(_) = form.try_next().await.unwrap_or(None) {}
                    if hash.is_some() {
                        Ok::<_, Infallible>(warp::reply::with_status(
                            "ok",
                            warp::http::StatusCode::OK,
                        ))
                    } else {
                        Ok(warp::reply::with_status(
                            "no hash",
                            warp::http::StatusCode::BAD_REQUEST,
                        ))
                    }
                },
            );

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("hash_test.txt");
        tokio::fs::write(&file_path, b"content").await.unwrap();

        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .upload_with_progress(
                &file_path,
                &format!("http://{}/upload", addr),
                Some("deadbeef"),
                tx,
            )
            .await;
        assert!(result.is_ok(), "upload with sha256 should succeed: {:?}", result);
    }

    // ── 6. Upload fails with 4xx → returns Err ───────────────────────────────
    #[tokio::test]
    async fn test_http_upload_server_error_returns_err() {
        let route = warp::post()
            .and(warp::path("upload"))
            .and(warp::multipart::form().max_length(1024 * 1024))
            .and_then(|mut form: warp::multipart::FormData| async move {
                use futures::TryStreamExt;
                while let Some(_) = form.try_next().await.unwrap_or(None) {}
                Ok::<_, Infallible>(warp::reply::with_status(
                    "rejected",
                    warp::http::StatusCode::FORBIDDEN,
                ))
            });

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("fail_test.txt");
        tokio::fs::write(&file_path, b"data").await.unwrap();

        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .upload_with_progress(&file_path, &format!("http://{}/upload", addr), None, tx)
            .await;
        assert!(result.is_err(), "upload should fail on 403");
    }

    // ── 7. Download from a real warp server ──────────────────────────────────
    #[tokio::test]
    async fn test_http_download_file() {
        let content = b"download me please";
        let route = warp::get()
            .and(warp::path("file"))
            .map(move || warp::reply::with_header(
                warp::reply::with_status(
                    content.to_vec(),
                    warp::http::StatusCode::OK,
                ),
                "content-type",
                "application/octet-stream",
            ));

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let dest_path = dir.path().join("downloaded.bin");
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .download_with_progress(
                &format!("http://{}/file", addr),
                &dest_path,
                None,
                tx,
            )
            .await;
        assert!(result.is_ok(), "download should succeed: {:?}", result);
        let downloaded = tokio::fs::read(&dest_path).await.unwrap();
        assert_eq!(downloaded, content);
    }

    // ── 8. Download verifies SHA-256 when header present ─────────────────────
    #[tokio::test]
    async fn test_http_download_sha256_mismatch_returns_err() {
        let content = b"some content";
        let route = warp::get()
            .and(warp::path("file"))
            .map(move || {
                warp::reply::with_header(
                    warp::reply::with_header(
                        warp::reply::with_status(content.to_vec(), warp::http::StatusCode::OK),
                        "x-file-hash",
                        "wrong_hash_value",
                    ),
                    "content-type",
                    "application/octet-stream",
                )
            });

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let dest_path = dir.path().join("mismatch.bin");
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .download_with_progress(
                &format!("http://{}/file", addr),
                &dest_path,
                None,
                tx,
            )
            .await;
        assert!(result.is_err(), "should fail on SHA-256 mismatch");
    }

    // ── 9. Range request for resume ───────────────────────────────────────────
    #[tokio::test]
    async fn test_http_resume_sends_range_header() {
        let route = warp::get()
            .and(warp::path("file"))
            .and(warp::header::<String>("range"))
            .map(|range: String| {
                assert!(range.starts_with("bytes="), "Range header format wrong");
                warp::reply::with_status(
                    b"partial".to_vec(),
                    warp::http::StatusCode::PARTIAL_CONTENT,
                )
            });

        let (addr, server) = warp::serve(route).bind_with_graceful_shutdown(
            ([127, 0, 0, 1], 0),
            async { tokio::time::sleep(std::time::Duration::from_secs(5)).await },
        );
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let local_path = dir.path().join("partial.bin");
        // Pre-write some bytes to simulate existing partial file
        tokio::fs::write(&local_path, b"existing").await.unwrap();

        let task = TransferTask {
            id: uuid::Uuid::new_v4(),
            source_path: local_path,
            destination: format!("http://{}/file", addr),
            is_upload: false,
            file_size: 100,
            sha256: None,
        };

        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht.resume_transfer(&task, 8, tx).await;
        assert!(result.is_ok(), "resume should succeed: {:?}", result);
    }

    // ── 10. protocol_name ────────────────────────────────────────────────────
    #[test]
    fn test_protocol_name() {
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        assert_eq!(ht.protocol_name(), "HTTP");
        assert!(ht.supports_resume());
    }
}

use crate::protocols::circuit_breaker::CircuitBreaker;
use crate::protocols::ratelimit::RateLimiter;
use crate::protocols::traits::{TransferProgress, TransferProtocol};
use crate::protocols::utils::send_progress;
use crate::core::resume::{ResumeState, ResumeStore};
use crate::core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use bytes::Bytes;
use memmap2::MmapOptions;
use rand::Rng;
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::Instant;
use tokio_util::io::ReaderStream;
use zeroize::Zeroizing;

pub struct HttpTransfer {
    client: Arc<Client>,
    config: HttpConfig,
    circuit_breaker: Arc<CircuitBreaker>,
    /// 可选的断点续传状态持久化存储，每完成一个分片后保存进度
    resume_store: Option<Arc<ResumeStore>>,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub chunk_size: usize,
    /// 发送方认证 Token（Bearer，drop 时自动清零内存）
    pub auth_token: Option<Zeroizing<String>>,
    /// 上传带宽限制（bytes/s），0 = 不限速
    pub upload_limit_bps: u64,
    /// 是否接受无效的 TLS 证书（用于连接自签名 HTTPS 服务端，默认 false）
    /// **已弃用**：生产环境请改用 `pinned_server_certs` 进行证书钉扎。
    pub accept_invalid_certs: bool,
    /// 服务端 DER 格式证书文件路径列表（用于证书钉扎）。
    /// 非空时只信任列表中的证书，忽略系统 CA，同时 `accept_invalid_certs` 无效。
    pub pinned_server_certs: Vec<PathBuf>,
    /// 并发分片上传路数（仅对 >5MB 文件生效），默认 4。
    /// 设为 1 等同于串行上传。
    pub concurrent_chunks: usize,
    /// 网络级别重连最大次数（区别于 per-chunk 重试），默认 5。
    /// 仅网络错误（连接拒绝/超时）触发重连，4xx 业务错误不触发。
    pub max_reconnect_attempts: u32,
    /// 重连指数退避基础等待时间（ms），默认 3000。
    /// 实际等待：base * 2^attempt，最大 60s。
    pub reconnect_base_delay_ms: u64,
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
            pinned_server_certs: vec![],
            concurrent_chunks: 4,
            max_reconnect_attempts: 5,
            reconnect_base_delay_ms: 3_000,
        }
    }
}

impl HttpTransfer {
    /// 创建一个新的 `HttpTransfer` 实例，内部会构造独立的 `reqwest::Client`（即独立连接池）。
    ///
    /// **注意**：每次调用 `new()` 都会分配一个新的连接池。若在同一进程中需要多个
    /// `HttpTransfer` 实例（例如同时上传到不同目标），建议改用
    /// [`HttpTransfer::new_with_client`]，共享同一个 `Arc<Client>`，以复用连接池、
    /// 减少文件描述符消耗并提升性能。
    pub fn new(config: HttpConfig) -> Result<Self> {
        let mut builder = Client::builder().timeout(Duration::from_secs(config.timeout_seconds));

        if !config.pinned_server_certs.is_empty() {
            // 证书钉扎模式：只信任指定证书，禁用系统 CA
            builder = builder.tls_built_in_root_certs(false);
            for cert_path in &config.pinned_server_certs {
                let der = std::fs::read(cert_path).map_err(|e| {
                    AeroSyncError::InvalidConfig(format!(
                        "Cannot read pinned cert {}: {}",
                        cert_path.display(),
                        e
                    ))
                })?;
                let cert = reqwest::Certificate::from_der(&der).map_err(|e| {
                    AeroSyncError::InvalidConfig(format!(
                        "Invalid DER cert {}: {}",
                        cert_path.display(),
                        e
                    ))
                })?;
                builder = builder.add_root_certificate(cert);
            }
        } else if config.accept_invalid_certs {
            tracing::warn!(
                "accept_invalid_certs=true is insecure; \
                use pinned_server_certs for production TLS pinning"
            );
            builder = builder.danger_accept_invalid_certs(true);
        }

        let client = builder
            .build()
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        Ok(Self {
            client: Arc::new(client),
            config,
            circuit_breaker: Arc::new(CircuitBreaker::with_defaults()),
            resume_store: None,
        })
    }

    /// 使用外部共享 client 构造，避免每次创建新连接池。
    pub fn new_with_client(client: Arc<Client>, config: HttpConfig) -> Self {
        Self {
            client,
            config,
            circuit_breaker: Arc::new(CircuitBreaker::with_defaults()),
            resume_store: None,
        }
    }

    /// 使用外部共享 client + ResumeStore 构造（每分片完成后持久化续传状态）。
    pub fn new_with_client_and_resume(
        client: Arc<Client>,
        config: HttpConfig,
        store: Arc<ResumeStore>,
    ) -> Self {
        Self {
            client,
            config,
            circuit_breaker: Arc::new(CircuitBreaker::with_defaults()),
            resume_store: Some(store),
        }
    }

    /// 判断错误是否属于网络级别故障（连接拒绝、超时、DNS 等），
    /// 返回 true 表示可重连，返回 false 表示业务错误（4xx）不重连。
    fn is_network_error(e: &AeroSyncError) -> bool {
        match e {
            AeroSyncError::Network(msg) => {
                let m = msg.to_lowercase();
                // 4xx 响应：认证失败/不存在等业务错误 → 不重连
                if m.contains("401")
                    || m.contains("403")
                    || m.contains("404")
                    || m.contains("400")
                    || m.contains("413")
                {
                    return false;
                }
                // 连接级别错误 → 重连
                true
            }
            AeroSyncError::FileIo(_) => false, // 本地 IO 错误不重连
            _ => false,
        }
    }

    /// 轮询 `{base_url}/health`，直到返回 200 或超时。
    /// 返回 true 表示服务已恢复，false 表示超时仍不健康。
    async fn wait_until_healthy(client: &Client, base_url: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let health_url = format!("{}/health", base_url);
        let poll_interval = Duration::from_secs(2);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            match client
                .get(&health_url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => return true,
                _ => {}
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// 分片上传入口：根据文件大小和配置自动选择串行或并发模式，
    /// 并在网络故障时进行指数退避重连（最多 `max_reconnect_attempts` 次）。
    ///
    /// - ≤5 MB：串行单流（`upload_chunked_serial`）
    /// - >5 MB：mmap 并发分片（`upload_chunked_concurrent`）
    /// - 4xx 业务错误不触发重连，直接返回
    ///
    /// `base_url`: 形如 `http://host:port`（不含路径）
    /// `state`:    ResumeState（含已完成分片，支持断点续传）
    #[tracing::instrument(skip(self, state, progress_tx), fields(task_id = %state.task_id, total_chunks = state.total_chunks))]
    pub async fn upload_chunked(
        &self,
        file_path: &std::path::Path,
        base_url: &str,
        state: &mut ResumeState,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        const SMALL_FILE_THRESHOLD: u64 = 5 * 1024 * 1024; // 5 MB
        let concurrency = if state.total_size <= SMALL_FILE_THRESHOLD {
            1
        } else {
            self.config.concurrent_chunks.max(1)
        };

        let max_reconnect = self.config.max_reconnect_attempts;
        let base_delay_ms = self.config.reconnect_base_delay_ms;
        let mut last_err = AeroSyncError::Network("upload_chunked: no attempts".to_string());

        for attempt in 0..=max_reconnect {
            let result = if concurrency <= 1 {
                self.upload_chunked_serial(file_path, base_url, state, progress_tx.clone())
                    .await
            } else {
                self.upload_chunked_concurrent(
                    file_path,
                    base_url,
                    state,
                    progress_tx.clone(),
                    concurrency,
                )
                .await
            };

            match result {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if !Self::is_network_error(&e) {
                        // 业务错误（4xx 等）直接返回，不重连
                        return Err(e);
                    }
                    last_err = e;
                    if attempt >= max_reconnect {
                        break;
                    }
                    // 指数退避：base * 2^attempt，最大 60s
                    let delay_ms = (base_delay_ms * (1u64 << attempt.min(5))).min(60_000);
                    tracing::warn!(
                        "Network error on attempt {}/{}, waiting {}ms before reconnect. pending_chunks={}",
                        attempt + 1, max_reconnect + 1, delay_ms,
                        state.pending_chunks().len()
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    // 等待服务器健康（最多60s）
                    if !Self::wait_until_healthy(&self.client, base_url, Duration::from_secs(60))
                        .await
                    {
                        tracing::warn!("Server still unhealthy after 60s, retrying anyway");
                    }
                }
            }
        }
        Err(last_err)
    }

    /// 串行分片上传（原有逻辑，用于小文件或 concurrent_chunks=1）。
    async fn upload_chunked_serial(
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
            "Chunked upload (serial): '{}' {} chunks (already done: {:?})",
            file_name,
            state.total_chunks,
            state.completed_chunks
        );

        for chunk_index in state.pending_chunks() {
            let offset = state.chunk_offset(chunk_index);
            let size = state.chunk_size_of(chunk_index);

            let mut file = File::open(file_path).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut raw = vec![0u8; size as usize];
            file.read_exact(&mut raw).await?;
            let buf = Bytes::from(raw);

            rate_limiter.consume(size).await;

            let chunk_url = format!(
                "{}/upload/chunk?task_id={}&chunk_index={}&total_chunks={}&filename={}&total_size={}&chunk_size={}",
                base_url, state.task_id, chunk_index, state.total_chunks,
                urlencoding::encode(&file_name), state.total_size, state.chunk_size
            );

            Self::upload_chunk_with_retry(
                &self.client,
                &chunk_url,
                buf,
                chunk_index,
                self.config.max_retries,
                self.config.auth_token.as_deref().map(|s| s.as_str()),
            )
            .await?;

            state.mark_chunk_done(chunk_index);
            if let Some(store) = &self.resume_store {
                if let Err(e) = store.save(state).await {
                    tracing::warn!(
                        "Failed to persist resume state after chunk {}: {}",
                        chunk_index,
                        e
                    );
                }
            }
            bytes_done += size;
            send_progress(&progress_tx, bytes_done, &start_time);
        }

        self.send_complete_request(base_url, state, &file_name)
            .await
    }

    /// mmap 并发分片上传（适用于 >5MB 大文件）。
    ///
    /// 用 `memmap2` 将文件只读映射到虚拟地址空间，多个 tokio task 并发读取不同分片偏移，
    /// 通过 `Semaphore` 控制最大并发数，避免打满带宽或连接池。
    async fn upload_chunked_concurrent(
        &self,
        file_path: &std::path::Path,
        base_url: &str,
        state: &mut ResumeState,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
        concurrency: usize,
    ) -> Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        tracing::info!(
            "Chunked upload (concurrent x{}): '{}' {} chunks",
            concurrency,
            file_name,
            state.total_chunks
        );

        // mmap 只读映射（需要 std::fs::File）
        let std_file = std::fs::File::open(file_path).map_err(AeroSyncError::FileIo)?;
        // SAFETY: 文件在整个上传过程中不被修改（只读映射）
        let mmap = Arc::new(unsafe {
            MmapOptions::new()
                .map(&std_file)
                .map_err(AeroSyncError::FileIo)?
        });

        let pending = state.pending_chunks();
        let total_pending = pending.len();
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let rate_limiter = Arc::new(RateLimiter::new(self.config.upload_limit_bps));

        // 结果 channel：(chunk_index, bytes_uploaded)
        let (result_tx, mut result_rx) =
            mpsc::unbounded_channel::<std::result::Result<(u32, u64), AeroSyncError>>();

        let start_time = Instant::now();

        // 为每个待上传分片 spawn 一个 task
        for chunk_index in pending {
            let offset = state.chunk_offset(chunk_index);
            let size = state.chunk_size_of(chunk_index);
            let mmap = Arc::clone(&mmap);
            let sem = Arc::clone(&semaphore);
            let rl = Arc::clone(&rate_limiter);
            let client = Arc::clone(&self.client);
            let auth_token = self.config.auth_token.as_deref().map(|s| s.to_string());
            let max_retries = self.config.max_retries;
            let tx = result_tx.clone();
            let chunk_url = format!(
                "{}/upload/chunk?task_id={}&chunk_index={}&total_chunks={}&filename={}&total_size={}&chunk_size={}",
                base_url, state.task_id, chunk_index, state.total_chunks,
                urlencoding::encode(&file_name), state.total_size, state.chunk_size
            );

            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                // mmap slice → Bytes（一次用户态拷贝，但避免了 seek+read syscall）
                let data = Bytes::copy_from_slice(&mmap[offset as usize..(offset + size) as usize]);
                rl.consume(size).await;
                let result = Self::upload_chunk_with_retry(
                    &client,
                    &chunk_url,
                    data,
                    chunk_index,
                    max_retries,
                    auth_token.as_deref(),
                )
                .await;
                let _ = tx.send(result.map(|_| (chunk_index, size)));
            });
        }
        drop(result_tx); // 关闭发送端，result_rx 会在所有 task 完成后耗尽

        // 收集结果，更新进度
        let mut completed = 0usize;
        let mut bytes_done = state.bytes_transferred();
        while let Some(res) = result_rx.recv().await {
            let (chunk_index, size) = res?;
            state.mark_chunk_done(chunk_index);
            if let Some(store) = &self.resume_store {
                if let Err(e) = store.save(state).await {
                    tracing::warn!(
                        "Failed to persist resume state after chunk {}: {}",
                        chunk_index,
                        e
                    );
                }
            }
            bytes_done += size;
            completed += 1;
            send_progress(&progress_tx, bytes_done, &start_time);
            tracing::debug!("Concurrent chunk {}/{} done", completed, total_pending);
        }

        self.send_complete_request(base_url, state, &file_name)
            .await
    }

    /// 带指数退避重试的单分片上传（串行/并发共用）。
    async fn upload_chunk_with_retry(
        client: &Client,
        chunk_url: &str,
        buf: Bytes,
        chunk_index: u32,
        max_retries: u32,
        auth_token: Option<&str>,
    ) -> Result<()> {
        let mut last_err = None;
        for attempt in 0..=max_retries {
            let mut req = client.post(chunk_url).body(buf.clone());
            if let Some(token) = auth_token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    last_err = Some(AeroSyncError::Network(format!(
                        "Chunk {} failed: {} - {}",
                        chunk_index, status, body
                    )));
                    tracing::warn!(
                        "Chunk {} attempt {}/{} failed: {}",
                        chunk_index,
                        attempt + 1,
                        max_retries + 1,
                        status
                    );
                }
                Err(e) => {
                    last_err = Some(AeroSyncError::Network(e.to_string()));
                    tracing::warn!(
                        "Chunk {} attempt {}/{} error: {}",
                        chunk_index,
                        attempt + 1,
                        max_retries + 1,
                        e
                    );
                }
            }
            if attempt < max_retries {
                let base_ms = 100u64 * (1u64 << attempt.min(8));
                let delay_ms = base_ms.min(30_000);
                let jitter_range = (delay_ms / 5) as i64;
                let jitter = rand::thread_rng().gen_range(-jitter_range..=jitter_range);
                let actual_ms = (delay_ms as i64 + jitter).max(50) as u64;
                tokio::time::sleep(Duration::from_millis(actual_ms)).await;
            }
        }
        Err(last_err.unwrap_or_else(|| {
            AeroSyncError::Network(format!("Chunk {} failed after all retries", chunk_index))
        }))
    }

    /// 发送 /upload/complete 合并请求（串行/并发共用）。
    async fn send_complete_request(
        &self,
        base_url: &str,
        state: &ResumeState,
        file_name: &str,
    ) -> Result<()> {
        let mut complete_url = format!(
            "{}/upload/complete?task_id={}&filename={}&total_chunks={}&total_size={}",
            base_url,
            state.task_id,
            urlencoding::encode(file_name),
            state.total_chunks,
            state.total_size,
        );
        if let Some(ref sha) = state.sha256 {
            complete_url.push_str(&format!("&sha256={}", sha));
        }
        let mut req = self.client.post(&complete_url);
        if let Some(token) = &self.config.auth_token {
            req = req.header("Authorization", format!("Bearer {}", token.as_str()));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("Complete request failed: {}", e)))?;
        if resp.status().is_success() {
            tracing::info!(
                "Chunked upload complete: {} ({} bytes)",
                file_name,
                state.total_size
            );
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

    #[tracing::instrument(skip(self, progress_tx), fields(file = ?file_path))]
    async fn upload_with_progress(
        &self,
        file_path: &std::path::Path,
        url: &str,
        sha256: Option<&str>,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // 熔断器检查：如果电路已开路，快速失败
        if !self.circuit_breaker.allow_request() {
            tracing::warn!("Circuit breaker OPEN, rejecting upload to {}", url);
            return Err(AeroSyncError::Network(format!(
                "Circuit breaker open, refusing upload to {}",
                url
            )));
        }

        let file_size = File::open(file_path).await?.metadata().await?.len();
        let start_time = Instant::now();

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        tracing::info!(
            "HTTP: Uploading '{}' ({} bytes) to {}",
            file_name,
            file_size,
            url
        );

        let mut last_err: Option<AeroSyncError> = None;

        for attempt in 0..=self.config.max_retries {
            // 每次重试重新打开文件（流只能消费一次）
            let file = File::open(file_path).await?;
            let stream = ReaderStream::new(file);
            let body = reqwest::Body::wrap_stream(stream);

            let file_part = reqwest::multipart::Part::stream_with_length(body, file_size)
                .file_name(file_name.clone())
                .mime_str("application/octet-stream")
                .map_err(|e| {
                    AeroSyncError::Network(format!("Failed to create multipart part: {}", e))
                })?;

            let form = reqwest::multipart::Form::new().part("file", file_part);

            let mut request = self.client.post(url).multipart(form);

            if let Some(token) = &self.config.auth_token {
                request = request.header("Authorization", format!("Bearer {}", token.as_str()));
            }
            if let Some(hash) = sha256 {
                request = request.header("X-File-Hash", hash);
                request = request.header("X-Hash-Algorithm", "sha256");
            }

            match request.send().await {
                Ok(resp) if resp.status().is_success() => {
                    send_progress(&progress_tx, file_size, &start_time);
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let mb_per_sec = if elapsed > 0.0 {
                        file_size as f64 / elapsed / (1024.0 * 1024.0)
                    } else {
                        0.0
                    };
                    tracing::info!(
                        "HTTP: Upload completed: {} bytes at {:.2} MB/s",
                        file_size,
                        mb_per_sec
                    );
                    self.circuit_breaker.record_success();
                    return Ok(());
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        "Upload attempt {}/{} failed: {} - {}",
                        attempt + 1,
                        self.config.max_retries + 1,
                        status,
                        body
                    );
                    last_err = Some(AeroSyncError::Network(format!(
                        "Upload failed: {} - {}",
                        status, body
                    )));
                }
                Err(e) => {
                    tracing::warn!(
                        "Upload attempt {}/{} error: {}",
                        attempt + 1,
                        self.config.max_retries + 1,
                        e
                    );
                    last_err = Some(AeroSyncError::Network(format!(
                        "Upload request failed: {}",
                        e
                    )));
                }
            }

            if attempt < self.config.max_retries {
                // 指数退避 + ±20% jitter：100ms, 200ms, 400ms...上限 30s
                let base_ms = 100u64 * (1u64 << attempt.min(8));
                let delay_ms = base_ms.min(30_000);
                let jitter_range = (delay_ms / 5) as i64;
                let jitter = rand::thread_rng().gen_range(-jitter_range..=jitter_range);
                let actual_ms = (delay_ms as i64 + jitter).max(50) as u64;
                tokio::time::sleep(Duration::from_millis(actual_ms)).await;
            }
        }

        self.circuit_breaker.record_failure();
        Err(last_err
            .unwrap_or_else(|| AeroSyncError::Network("Upload failed: no attempts made".into())))
    }

    #[tracing::instrument(skip(self, progress_tx), fields(url = url))]
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
            request = request.header("Authorization", format!("Bearer {}", token.as_str()));
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
            send_progress(&progress_tx, bytes_transferred, &start_time);
        }
        file.flush().await?;

        // 完成后校验 SHA-256（优先用调用方期望的 hash，其次用服务端提供的）
        let hash_to_check = expected_sha256.map(|s| s.to_string()).or(server_hash);
        if let Some(expected) = hash_to_check {
            use sha2::{Digest, Sha256};
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
            let completed_chunks: Vec<u32> = offset
                .checked_div(chunk_size)
                .map(|n| (0..(n as u32)).collect())
                .unwrap_or_default();

            let mut state = crate::core::resume::ResumeState::new(
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
            let mut request = self
                .client
                .get(&task.destination)
                .header("Range", format!("bytes={}-", offset));
            if let Some(token) = &self.config.auth_token {
                request = request.header("Authorization", format!("Bearer {}", token.as_str()));
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
                send_progress(&progress_tx, bytes_transferred, &start_time);
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

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });

        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("upload_test.bin");
        tokio::fs::write(&file_path, b"hello streaming world")
            .await
            .unwrap();

        let url = format!("http://{}/upload", addr);
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht.upload_with_progress(&file_path, &url, None, tx).await;
        assert!(result.is_ok(), "upload should succeed: {:?}", result);
    }

    // ── 4. Upload attaches Authorization header ───────────────────────────────
    #[tokio::test]
    async fn test_http_upload_attaches_auth_header() {
        let route = warp::post()
            .and(warp::path("upload"))
            .and(warp::header::<String>("authorization"))
            .and(warp::multipart::form().max_length(1024 * 1024))
            .and_then(
                |auth: String, mut form: warp::multipart::FormData| async move {
                    use futures::TryStreamExt;
                    while form.try_next().await.unwrap_or(None).is_some() {}
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
                },
            );

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("auth_test.txt");
        tokio::fs::write(&file_path, b"auth data").await.unwrap();

        let cfg = HttpConfig {
            auth_token: Some("test-token".to_string().into()),
            ..Default::default()
        };
        let ht = HttpTransfer::new(cfg).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .upload_with_progress(&file_path, &format!("http://{}/upload", addr), None, tx)
            .await;
        assert!(
            result.is_ok(),
            "upload with auth token should succeed: {:?}",
            result
        );
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
                    while form.try_next().await.unwrap_or(None).is_some() {}
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

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
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
        assert!(
            result.is_ok(),
            "upload with sha256 should succeed: {:?}",
            result
        );
    }

    // ── 6. Upload fails with 4xx → returns Err ───────────────────────────────
    #[tokio::test]
    async fn test_http_upload_server_error_returns_err() {
        let route = warp::post()
            .and(warp::path("upload"))
            .and(warp::multipart::form().max_length(1024 * 1024))
            .and_then(|mut form: warp::multipart::FormData| async move {
                use futures::TryStreamExt;
                while form.try_next().await.unwrap_or(None).is_some() {}
                Ok::<_, Infallible>(warp::reply::with_status(
                    "rejected",
                    warp::http::StatusCode::FORBIDDEN,
                ))
            });

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
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
        let route = warp::get().and(warp::path("file")).map(move || {
            warp::reply::with_header(
                warp::reply::with_status(content.to_vec(), warp::http::StatusCode::OK),
                "content-type",
                "application/octet-stream",
            )
        });

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let dest_path = dir.path().join("downloaded.bin");
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .download_with_progress(&format!("http://{}/file", addr), &dest_path, None, tx)
            .await;
        assert!(result.is_ok(), "download should succeed: {:?}", result);
        let downloaded = tokio::fs::read(&dest_path).await.unwrap();
        assert_eq!(downloaded, content);
    }

    // ── 8. Download verifies SHA-256 when header present ─────────────────────
    #[tokio::test]
    async fn test_http_download_sha256_mismatch_returns_err() {
        let content = b"some content";
        let route = warp::get().and(warp::path("file")).map(move || {
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

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let dest_path = dir.path().join("mismatch.bin");
        let ht = HttpTransfer::new(HttpConfig::default()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = ht
            .download_with_progress(&format!("http://{}/file", addr), &dest_path, None, tx)
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

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
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

    // ── 11. Certificate pinning ───────────────────────────────────────────────
    #[test]
    fn test_http_pinned_cert_field_in_default() {
        let cfg = HttpConfig::default();
        assert!(cfg.pinned_server_certs.is_empty());
    }

    #[test]
    fn test_http_new_with_nonexistent_cert_returns_err() {
        let config = HttpConfig {
            pinned_server_certs: vec![PathBuf::from("/nonexistent/cert.der")],
            ..HttpConfig::default()
        };
        let result = HttpTransfer::new(config);
        assert!(result.is_err());
        match result {
            Err(crate::core::AeroSyncError::InvalidConfig(msg)) => {
                assert!(msg.contains("Cannot read pinned cert"), "Got: {}", msg);
            }
            Err(e) => panic!("Wrong error type: {:?}", e),
            Ok(_) => panic!("Should have failed"),
        }
    }

    #[test]
    fn test_http_new_with_invalid_der_returns_err() {
        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("bad.der");
        std::fs::write(&cert_path, b"not a real DER certificate").unwrap();
        let config = HttpConfig {
            pinned_server_certs: vec![cert_path],
            ..HttpConfig::default()
        };
        let result = HttpTransfer::new(config);
        assert!(result.is_err(), "should reject malformed pinned cert");
        let msg = format!("{:?}", result.err().unwrap()).to_lowercase();
        assert!(
            msg.contains("invalid")
                || msg.contains("der")
                || msg.contains("cert")
                || msg.contains("badencoding"),
            "unexpected error message: {msg}",
        );
    }

    // Helper: build a mock upload server that counts chunk POSTs
    fn make_counting_server(
        chunk_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) -> impl warp::Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        use std::sync::atomic::Ordering;
        let chunk_route = warp::post()
            .and(warp::path!("upload" / "chunk"))
            .and(warp::body::bytes())
            .map(move |_b: bytes::Bytes| {
                chunk_count.fetch_add(1, Ordering::Relaxed);
                warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"ok": true})),
                    warp::http::StatusCode::OK,
                )
            });
        let complete_route = warp::post()
            .and(warp::path!("upload" / "complete"))
            .map(|| {
                warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"status": "complete"})),
                    warp::http::StatusCode::OK,
                )
            });
        chunk_route.or(complete_route)
    }

    // ── 13. concurrent_chunks field default ─────────────────────────────────
    #[test]
    fn test_http_config_concurrent_chunks_default() {
        let cfg = HttpConfig::default();
        assert_eq!(cfg.concurrent_chunks, 4);
    }

    // ── 14. Small file (<= 5MB) routes to serial upload ──────────────────────
    #[tokio::test]
    async fn test_upload_small_file_uses_serial_path() {
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc as StdArc;

        let chunk_count = StdArc::new(AtomicU32::new(0));
        let route = make_counting_server(StdArc::clone(&chunk_count));

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("small.bin");
        // 1 MB — below 5 MB threshold → serial, chunk_size=256KB → 4 chunks
        let data = vec![0xABu8; 1024 * 1024];
        tokio::fs::write(&file_path, &data).await.unwrap();

        let cfg = HttpConfig {
            chunk_size: 256 * 1024,
            concurrent_chunks: 4,
            ..HttpConfig::default()
        };
        let ht = HttpTransfer::new(cfg).unwrap();
        let mut state = crate::core::resume::ResumeState::new(
            uuid::Uuid::new_v4(),
            file_path.clone(),
            format!("http://{}", addr),
            data.len() as u64,
            256 * 1024,
            None,
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let base = format!("http://{}", addr);
        let result = ht.upload_chunked(&file_path, &base, &mut state, tx).await;
        assert!(result.is_ok(), "serial upload should succeed: {:?}", result);
        assert_eq!(chunk_count.load(std::sync::atomic::Ordering::Relaxed), 4);
    }

    // ── 15. Large file (> 5MB) routes to concurrent upload ───────────────────
    #[tokio::test]
    async fn test_upload_large_file_uses_concurrent_path() {
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc as StdArc;

        let chunk_count = StdArc::new(AtomicU32::new(0));
        let route = make_counting_server(StdArc::clone(&chunk_count));

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("large.bin");
        // 6 MB — above 5 MB threshold → concurrent, chunk_size=1MB → 6 chunks
        let data = vec![0xCDu8; 6 * 1024 * 1024];
        tokio::fs::write(&file_path, &data).await.unwrap();

        let cfg = HttpConfig {
            chunk_size: 1024 * 1024,
            concurrent_chunks: 3,
            ..HttpConfig::default()
        };
        let ht = HttpTransfer::new(cfg).unwrap();
        let mut state = crate::core::resume::ResumeState::new(
            uuid::Uuid::new_v4(),
            file_path.clone(),
            format!("http://{}", addr),
            data.len() as u64,
            1024 * 1024,
            None,
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let base = format!("http://{}", addr);
        let result = ht.upload_chunked(&file_path, &base, &mut state, tx).await;
        assert!(
            result.is_ok(),
            "concurrent upload should succeed: {:?}",
            result
        );
        assert_eq!(chunk_count.load(std::sync::atomic::Ordering::Relaxed), 6);
    }

    // ── 16. Concurrent upload resumes from partial state ─────────────────────
    #[tokio::test]
    async fn test_concurrent_upload_resumes_partial() {
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc as StdArc;

        let chunk_count = StdArc::new(AtomicU32::new(0));
        let route = make_counting_server(StdArc::clone(&chunk_count));

        let (addr, server) = warp::serve(route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("resume.bin");
        // 8 chunks of 1 MB, first 4 already done → only 4 uploaded
        let data = vec![0xEFu8; 8 * 1024 * 1024];
        tokio::fs::write(&file_path, &data).await.unwrap();

        let cfg = HttpConfig {
            chunk_size: 1024 * 1024,
            concurrent_chunks: 4,
            ..HttpConfig::default()
        };
        let ht = HttpTransfer::new(cfg).unwrap();
        let mut state = crate::core::resume::ResumeState::new(
            uuid::Uuid::new_v4(),
            file_path.clone(),
            format!("http://{}", addr),
            data.len() as u64,
            1024 * 1024,
            None,
        );
        for i in 0u32..4 {
            state.mark_chunk_done(i);
        }
        let (tx, _rx) = mpsc::unbounded_channel();
        let base = format!("http://{}", addr);
        let result = ht.upload_chunked(&file_path, &base, &mut state, tx).await;
        assert!(
            result.is_ok(),
            "resumed upload should succeed: {:?}",
            result
        );
        assert_eq!(chunk_count.load(std::sync::atomic::Ordering::Relaxed), 4);
    }

    // ── 16. is_network_error：业务错误返回 false，连接错误返回 true ────────────
    #[test]
    fn test_is_network_error_business_errors_return_false() {
        // 4xx 业务错误不触发重连
        for code in &["401", "403", "404", "400", "413"] {
            let e = AeroSyncError::Network(format!("HTTP {} Unauthorized", code));
            assert!(
                !HttpTransfer::is_network_error(&e),
                "Expected false for {}",
                code
            );
        }
    }

    #[test]
    fn test_is_network_error_connection_errors_return_true() {
        // 网络级别错误触发重连
        let e = AeroSyncError::Network("connection refused".to_string());
        assert!(
            HttpTransfer::is_network_error(&e),
            "connection refused should be network error"
        );

        let e2 = AeroSyncError::Network("timeout waiting for response".to_string());
        assert!(
            HttpTransfer::is_network_error(&e2),
            "timeout should be network error"
        );
    }

    #[test]
    fn test_is_network_error_file_io_returns_false() {
        let e = AeroSyncError::FileIo(std::io::Error::new(std::io::ErrorKind::NotFound, "no file"));
        assert!(
            !HttpTransfer::is_network_error(&e),
            "FileIo should not trigger reconnect"
        );
    }

    // ── 17. wait_until_healthy：服务器可用时立即返回 true ─────────────────────
    #[tokio::test]
    async fn test_wait_until_healthy_returns_true_when_server_ok() {
        let health_route = warp::get()
            .and(warp::path("health"))
            .map(|| warp::reply::with_status("ok", warp::http::StatusCode::OK));

        let (addr, server) = warp::serve(health_route)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            });
        tokio::spawn(server);

        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);
        let healthy =
            HttpTransfer::wait_until_healthy(&client, &base, std::time::Duration::from_secs(5))
                .await;
        assert!(healthy, "should return true when /health returns 200");
    }

    // ── 18. upload_chunked 重连：第一次失败后成功重传 ─────────────────────────
    #[tokio::test]
    async fn test_upload_chunked_reconnects_on_network_error() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // 服务器：前 N 个 chunk 请求返回 503（触发重连），之后正常
        let fail_count = StdArc::new(AtomicU32::new(0));
        let fail_count2 = StdArc::clone(&fail_count);

        let chunk_route = warp::post()
            .and(warp::path!("upload" / "chunk"))
            .and(warp::body::bytes())
            .map(move |_b: bytes::Bytes| {
                let n = fail_count2.fetch_add(1, Ordering::Relaxed);
                // 第一次返回 503，后续正常
                if n == 0 {
                    warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"error": "service unavailable"})),
                        warp::http::StatusCode::SERVICE_UNAVAILABLE,
                    )
                } else {
                    warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({"ok": true})),
                        warp::http::StatusCode::OK,
                    )
                }
            });
        let complete_route = warp::post()
            .and(warp::path!("upload" / "complete"))
            .map(|| {
                warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"status": "complete"})),
                    warp::http::StatusCode::OK,
                )
            });
        let health_route = warp::get()
            .and(warp::path("health"))
            .map(|| warp::reply::with_status("ok", warp::http::StatusCode::OK));
        let routes = chunk_route.or(complete_route).or(health_route);

        let (addr, server) = warp::serve(routes)
            .bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await
            });
        tokio::spawn(server);

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("reconnect_test.bin");
        let data = vec![0xAAu8; 512 * 1024]; // 512 KB → 1 chunk（< 5MB，串行模式）
        tokio::fs::write(&file_path, &data).await.unwrap();

        let cfg = HttpConfig {
            chunk_size: 1024 * 1024,
            max_retries: 0, // 单 chunk 不重试（让外层重连处理）
            max_reconnect_attempts: 3,
            reconnect_base_delay_ms: 10, // 测试中缩短等待
            ..HttpConfig::default()
        };
        let ht = HttpTransfer::new(cfg).unwrap();
        let mut state = crate::core::resume::ResumeState::new(
            uuid::Uuid::new_v4(),
            file_path.clone(),
            format!("http://{}", addr),
            data.len() as u64,
            1024 * 1024,
            None,
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let base = format!("http://{}", addr);
        let result = ht.upload_chunked(&file_path, &base, &mut state, tx).await;
        // 503 被识别为网络错误 → 触发重连 → 第二次成功
        assert!(result.is_ok(), "reconnect should succeed: {:?}", result);
    }
}

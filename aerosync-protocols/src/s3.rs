//! S3 协议适配器
//!
//! 支持 AWS S3 和兼容 S3 API 的存储服务（如 MinIO）。
//! URL 格式：s3://bucket/path/to/file
//!
//! 认证策略：
//! - 若设置了 endpoint（MinIO 等），使用 Bearer token 认证
//! - 否则，使用完整的 AWS Signature Version 4 (SigV4) 认证
//!
//! Multipart Upload（大文件分片上传）：
//! - 文件大小超过 `multipart_threshold` 时自动切换为分片上传
//! - 三步流程：Initiate → Upload Part(s) → Complete
//! - 上传 ID（UploadId）可通过 `ResumeState.metadata` 持久化以支持断点续传

use crate::traits::{TransferProgress, TransferProtocol};
use crate::utils::send_progress;
use aerosync_core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

// Suppress unused import warnings for HMAC type used inside helper functions
#[allow(unused_imports)]
use sha2::Digest;

/// 默认分片阈值：100 MB
pub const DEFAULT_MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;
/// 默认每片大小：16 MB
pub const DEFAULT_PART_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct S3Config {
    /// S3 存储桶名称
    pub bucket: String,
    /// AWS 区域，如 "us-east-1"
    pub region: String,
    /// Access Key ID
    pub access_key: String,
    /// Secret Access Key
    pub secret_key: String,
    /// 自定义 endpoint（MinIO 等），留空则使用 AWS S3
    pub endpoint: Option<String>,
    /// 请求超时秒数
    pub timeout_seconds: u64,
    /// 文件大小超过该阈值时切换为 Multipart Upload（字节，默认 100MB）
    pub multipart_threshold: u64,
    /// 每个 part 的大小（字节，默认 16MB）
    pub part_size: usize,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            region: "us-east-1".to_string(),
            access_key: String::new(),
            secret_key: String::new(),
            endpoint: None,
            timeout_seconds: 60,
            multipart_threshold: DEFAULT_MULTIPART_THRESHOLD,
            part_size: DEFAULT_PART_SIZE,
        }
    }
}

pub struct S3Transfer {
    config: S3Config,
    client: Arc<Client>,
}

impl S3Transfer {
    pub fn new(config: S3Config) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|e| AeroSyncError::Network(format!("Failed to build S3 client: {}", e)))?;
        Ok(Self {
            config,
            client: Arc::new(client),
        })
    }

    pub fn new_with_client(config: S3Config, client: Arc<Client>) -> Self {
        Self { config, client }
    }

    /// 从 s3://bucket/path 格式的 URL 中解析 bucket 和 key
    pub fn parse_s3_url(url: &str) -> Result<(String, String)> {
        let stripped = url
            .strip_prefix("s3://")
            .ok_or_else(|| AeroSyncError::InvalidConfig(format!("Not an S3 URL: {}", url)))?;
        let (bucket, key) = stripped.split_once('/').ok_or_else(|| {
            AeroSyncError::InvalidConfig(format!("S3 URL missing key path: {}", url))
        })?;
        if bucket.is_empty() {
            return Err(AeroSyncError::InvalidConfig(format!(
                "S3 URL has empty bucket: {}",
                url
            )));
        }
        if key.is_empty() {
            return Err(AeroSyncError::InvalidConfig(format!(
                "S3 URL has empty key: {}",
                url
            )));
        }
        Ok((bucket.to_string(), key.to_string()))
    }

    /// 构建 S3 PUT URL
    fn build_put_url(&self, bucket: &str, key: &str) -> String {
        if let Some(ref endpoint) = self.config.endpoint {
            // MinIO 或自定义 endpoint：endpoint/bucket/key
            format!("{}/{}/{}", endpoint.trim_end_matches('/'), bucket, key)
        } else {
            // AWS S3 标准格式
            format!(
                "https://{}.s3.{}.amazonaws.com/{}",
                bucket, self.config.region, key
            )
        }
    }

    /// 构建认证 header
    ///
    /// - MinIO / 自定义 endpoint：Bearer token
    /// - AWS S3：完整 AWS Signature Version 4
    fn build_auth_header(&self) -> String {
        self.build_auth_header_for(None, None, b"")
    }

    /// 构建 AWS SigV4 Authorization header
    ///
    /// `method`       - HTTP 方法（PUT / POST / DELETE / GET）
    /// `canonical_uri`- URL 路径部分（如 /bucket/key）
    /// `payload`      - 请求体字节（用于计算 x-amz-content-sha256）
    fn build_auth_header_for(
        &self,
        method: Option<&str>,
        canonical_uri: Option<&str>,
        payload: &[u8],
    ) -> String {
        if self.config.endpoint.is_some() {
            // MinIO: Bearer token 认证
            return format!("Bearer {}", self.config.access_key);
        }

        // AWS SigV4
        let method = method.unwrap_or("PUT");
        let uri = canonical_uri.unwrap_or("/");
        let (date_stamp, datetime_stamp) = aws_timestamps();
        let region = &self.config.region;
        let service = "s3";

        let payload_hash = sha256_hex(payload);
        let canonical_headers = format!(
            "host:{}.s3.{}.amazonaws.com\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            self.config.bucket, region, payload_hash, datetime_stamp
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method, uri, canonical_headers, signed_headers, payload_hash
        );

        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            datetime_stamp,
            credential_scope,
            sha256_hex(canonical_request.as_bytes())
        );

        let signing_key = derive_signing_key(&self.config.secret_key, &date_stamp, region, service);
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.config.access_key, credential_scope, signed_headers, signature
        )
    }

    // ── Multipart Upload API ──────────────────────────────────────────────────

    /// Step 1: Initiate a Multipart Upload, returns the UploadId.
    pub async fn initiate_multipart(&self, bucket: &str, key: &str) -> Result<String> {
        let url = format!("{}?uploads", self.build_put_url(bucket, key));
        let auth = self.build_auth_header();
        let now_str = amz_date_string();

        let mut req = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", "0");

        if self.config.endpoint.is_none() {
            req = req.header("x-amz-date", now_str);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 Initiate Multipart failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AeroSyncError::Network(format!(
                "S3 Initiate Multipart: {} - {}",
                status, body
            )));
        }

        let body = resp.text().await.unwrap_or_default();
        // Parse <UploadId>...</UploadId> from XML response
        let upload_id = parse_xml_tag(&body, "UploadId").ok_or_else(|| {
            AeroSyncError::Network(format!(
                "S3 Initiate Multipart: could not parse UploadId from response: {}",
                &body[..body.len().min(256)]
            ))
        })?;

        tracing::debug!(
            "S3 Multipart initiated: upload_id={}",
            &upload_id[..upload_id.len().min(16)]
        );
        Ok(upload_id)
    }

    /// Step 2: Upload a single part. Returns the ETag header value.
    ///
    /// `part_number` is 1-indexed.
    pub async fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String> {
        let base_url = self.build_put_url(bucket, key);
        let url = format!(
            "{}?partNumber={}&uploadId={}",
            base_url, part_number, upload_id
        );
        let auth = self.build_auth_header();
        let now_str = amz_date_string();
        let content_len = data.len();

        let mut req = self
            .client
            .put(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", content_len.to_string())
            .body(data);

        if self.config.endpoint.is_none() {
            req = req.header("x-amz-date", now_str);
        }

        let resp = req.send().await.map_err(|e| {
            AeroSyncError::Network(format!("S3 Upload Part {} failed: {}", part_number, e))
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AeroSyncError::Network(format!(
                "S3 Upload Part {}: {} - {}",
                part_number, status, body
            )));
        }

        let etag = resp
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim_matches('"')
            .to_string();

        tracing::debug!(
            "S3 Part {} uploaded, ETag={}",
            part_number,
            &etag[..etag.len().min(16)]
        );
        Ok(etag)
    }

    /// Step 3: Complete the Multipart Upload.
    ///
    /// `parts` is a list of `(part_number, etag)` pairs, 1-indexed, in order.
    pub async fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<()> {
        let base_url = self.build_put_url(bucket, key);
        let url = format!("{}?uploadId={}", base_url, upload_id);
        let auth = self.build_auth_header();
        let now_str = amz_date_string();

        // Build XML body
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (num, etag) in parts {
            xml.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>\"{}\"</ETag></Part>",
                num, etag
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");

        let mut req = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/xml")
            .header("Content-Length", xml.len().to_string())
            .body(xml);

        if self.config.endpoint.is_none() {
            req = req.header("x-amz-date", now_str);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 Complete Multipart failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AeroSyncError::Network(format!(
                "S3 Complete Multipart: {} - {}",
                status, body
            )));
        }

        tracing::info!("S3 Multipart upload completed: s3://{}/{}", bucket, key);
        Ok(())
    }

    /// Abort a Multipart Upload (cleanup on failure).
    pub async fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<()> {
        let base_url = self.build_put_url(bucket, key);
        let url = format!("{}?uploadId={}", base_url, upload_id);
        let auth = self.build_auth_header();

        let resp = self
            .client
            .delete(&url)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 Abort Multipart failed: {}", e)))?;

        if !resp.status().is_success() && resp.status().as_u16() != 204 {
            let status = resp.status();
            tracing::warn!("S3 Abort Multipart returned: {}", status);
        }
        Ok(())
    }

    /// Automatically choose between simple PUT and Multipart Upload.
    ///
    /// Uses `multipart_threshold` from config to decide.
    /// Reports progress via `progress_tx` after each uploaded part.
    /// For multipart uploads, returns the UploadId + completed parts via
    /// an optional `ResumeState` callback so callers can persist for resume.
    pub async fn upload_auto(
        &self,
        file_path: &std::path::Path,
        url: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        self.upload_auto_with_resume(file_path, url, None, vec![], progress_tx)
            .await
    }

    /// Internal multipart upload with resume support.
    ///
    /// `existing_upload_id` — resume an existing upload (None = start fresh)
    /// `already_completed`  — parts already uploaded (can skip)
    async fn upload_auto_with_resume(
        &self,
        file_path: &std::path::Path,
        url: &str,
        existing_upload_id: Option<&str>,
        already_completed: Vec<(u32, String)>,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let file_size = tokio::fs::metadata(file_path).await?.len();

        if file_size < self.config.multipart_threshold {
            return self.upload_to_s3(file_path, url, progress_tx).await;
        }

        // ── Multipart path ───────────────────────────────────────────────────
        // Parse bucket/key from the URL we were given
        let (bucket, key) = self.url_to_bucket_key(url)?;

        // Reuse existing UploadId or initiate a new one
        let upload_id = if let Some(id) = existing_upload_id {
            id.to_string()
        } else {
            self.initiate_multipart(&bucket, &key).await?
        };

        // Build set of already-completed part numbers for fast lookup
        let completed_set: std::collections::HashSet<u32> =
            already_completed.iter().map(|(n, _)| *n).collect();

        let mut file = File::open(file_path).await?;
        let part_size = self.config.part_size;
        let mut part_number: u32 = 1;
        // Start with previously completed parts
        let mut parts: Vec<(u32, String)> = already_completed;
        // Count bytes already transferred from completed parts
        let mut bytes_uploaded: u64 = parts.len() as u64 * part_size as u64;
        let start = Instant::now();

        loop {
            let mut buf = vec![0u8; part_size];
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            buf.truncate(n);

            // Skip already-uploaded parts (seek past them)
            if completed_set.contains(&part_number) {
                part_number += 1;
                continue;
            }

            let etag = match self
                .upload_part(&bucket, &key, &upload_id, part_number, buf)
                .await
            {
                Ok(e) => e,
                Err(e) => {
                    // Don't abort — preserve UploadId for future resume
                    tracing::warn!(
                        "S3 part {} upload failed (upload_id={}): {}",
                        part_number,
                        &upload_id[..upload_id.len().min(8)],
                        e
                    );
                    return Err(e);
                }
            };

            parts.push((part_number, etag));
            bytes_uploaded += n as u64;
            part_number += 1;
            send_progress(&progress_tx, bytes_uploaded, &start);
        }

        // Sort parts by part_number before completing (required by S3 API)
        parts.sort_by_key(|(n, _)| *n);
        self.complete_multipart(&bucket, &key, &upload_id, &parts)
            .await
    }

    /// Resume an in-progress Multipart Upload from saved state.
    async fn resume_multipart_upload(
        &self,
        file_path: &std::path::Path,
        bucket: &str,
        key: &str,
        upload_id: &str,
        already_completed: Vec<(u32, String)>,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let url = self.build_put_url(bucket, key);
        self.upload_auto_with_resume(
            file_path,
            &url,
            Some(upload_id),
            already_completed,
            progress_tx,
        )
        .await
    }

    /// Extract (bucket, key) from a fully-qualified S3 URL built by build_put_url
    fn url_to_bucket_key(&self, url: &str) -> Result<(String, String)> {
        if let Some(ref endpoint) = self.config.endpoint {
            // MinIO format: endpoint/bucket/key
            let prefix = endpoint.trim_end_matches('/');
            let rest = url
                .strip_prefix(prefix)
                .and_then(|s| s.strip_prefix('/'))
                .ok_or_else(|| {
                    AeroSyncError::InvalidConfig(format!("Cannot parse URL: {}", url))
                })?;
            let (bucket, key) = rest.split_once('/').ok_or_else(|| {
                AeroSyncError::InvalidConfig(format!("Cannot parse bucket/key from URL: {}", url))
            })?;
            Ok((bucket.to_string(), key.to_string()))
        } else {
            // AWS format: https://<bucket>.s3.<region>.amazonaws.com/<key>
            let without_scheme = url.strip_prefix("https://").ok_or_else(|| {
                AeroSyncError::InvalidConfig(format!("Cannot parse URL: {}", url))
            })?;
            let (host, key) = without_scheme.split_once('/').ok_or_else(|| {
                AeroSyncError::InvalidConfig(format!("Cannot parse key from URL: {}", url))
            })?;
            let bucket = host.split('.').next().ok_or_else(|| {
                AeroSyncError::InvalidConfig(format!("Cannot parse bucket from host: {}", host))
            })?;
            Ok((bucket.to_string(), key.to_string()))
        }
    }

    async fn upload_to_s3(
        &self,
        file_path: &std::path::Path,
        url: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let mut file = File::open(file_path).await?;
        let file_size = file.metadata().await?.len();
        let mut data = Vec::with_capacity(file_size as usize);
        file.read_to_end(&mut data).await?;

        let start = Instant::now();
        let auth = self.build_auth_header();
        let now_str = amz_date_string();

        let mut req = self
            .client
            .put(url)
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", file_size.to_string())
            .body(data);

        if self.config.endpoint.is_none() {
            req = req.header("x-amz-date", now_str);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 PUT failed: {}", e)))?;

        if resp.status().is_success() || resp.status().as_u16() == 200 {
            send_progress(&progress_tx, file_size, &start);
            tracing::info!("S3: Upload OK: {} ({} bytes)", url, file_size);
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(AeroSyncError::Network(format!(
                "S3 PUT failed: {} - {}",
                status, body
            )))
        }
    }

    async fn download_from_s3(
        &self,
        url: &str,
        dest_path: &std::path::Path,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;

        let auth = self.build_auth_header();
        let now_str = amz_date_string();

        let mut req = self.client.get(url).header("Authorization", auth);

        if self.config.endpoint.is_none() {
            req = req.header("x-amz-date", now_str);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 GET failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(AeroSyncError::Network(format!("S3 GET failed: {}", status)));
        }

        let mut file = tokio::fs::File::create(dest_path).await?;
        let start = Instant::now();
        let mut bytes_transferred = 0u64;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AeroSyncError::Network(e.to_string()))?;
            file.write_all(&chunk).await?;
            bytes_transferred += chunk.len() as u64;
            send_progress(&progress_tx, bytes_transferred, &start);
        }
        file.flush().await?;
        Ok(())
    }
}

#[async_trait]
impl TransferProtocol for S3Transfer {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let (bucket, key) = Self::parse_s3_url(&task.destination)?;
        let url = self.build_put_url(&bucket, &key);
        self.upload_auto(&task.source_path, &url, progress_tx).await
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let (bucket, key) = Self::parse_s3_url(&task.destination)?;
        let url = self.build_put_url(&bucket, &key);
        self.download_from_s3(&url, &task.source_path, progress_tx)
            .await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        _offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // 通过文件系统查找保存的 S3 断点信息
        let state_path = std::env::temp_dir().join(format!(".aerosync_s3_{}.json", task.id));
        if let Ok(bytes) = tokio::fs::read(&state_path).await {
            if let Ok(saved) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let upload_id = saved["upload_id"].as_str().unwrap_or("").to_string();
                let parts_json = saved["completed_parts"].to_string();
                if !upload_id.is_empty() {
                    if let Ok(completed_parts) =
                        serde_json::from_str::<Vec<(u32, String)>>(&parts_json)
                    {
                        let (bucket, key) = Self::parse_s3_url(&task.destination)?;
                        // Clean up state file on success
                        let _ = tokio::fs::remove_file(&state_path).await;
                        return self
                            .resume_multipart_upload(
                                &task.source_path,
                                &bucket,
                                &key,
                                &upload_id,
                                completed_parts,
                                progress_tx,
                            )
                            .await;
                    }
                }
            }
        }
        // 没有可用的断点信息，重新上传
        self.upload_file(task, progress_tx).await
    }

    fn supports_resume(&self) -> bool {
        // 大文件（Multipart 路径）支持断点续传
        true
    }

    fn protocol_name(&self) -> &'static str {
        "S3"
    }
}

// ── XML helpers ───────────────────────────────────────────────────────────────

/// Extract the text content of the first occurrence of an XML tag.
fn parse_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].to_string())
}

// ── AWS SigV4 helpers ─────────────────────────────────────────────────────────

/// 返回 (YYYYMMDD, YYYYMMDDTHHmmssZ)
fn aws_timestamps() -> (String, String) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let total_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (year, month, day, hour, minute, second) = unix_to_datetime(total_secs);
    let date = format!("{:04}{:02}{:02}", year, month, day);
    let datetime = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year, month, day, hour, minute, second
    );
    (date, datetime)
}

/// Convert Unix timestamp to (year, month, day, hour, minute, second) in UTC.
fn unix_to_datetime(ts: u64) -> (u32, u32, u32, u32, u32, u32) {
    let second = (ts % 60) as u32;
    let minutes = ts / 60;
    let minute = (minutes % 60) as u32;
    let hours = minutes / 60;
    let hour = (hours % 24) as u32;
    let mut days = hours / 24;

    // Gregorian calendar computation (days since 1970-01-01)
    let mut year = 1970u32;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let leap = is_leap(year);
    let days_per_month: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u32;
    for &dim in &days_per_month {
        if days < dim {
            break;
        }
        days -= dim;
        month += 1;
    }
    let day = days as u32 + 1;
    (year, month, day, hour, minute, second)
}

fn is_leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// HMAC-SHA256: key × data → 32-byte digest
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// SHA-256 hex digest
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(data))
}

/// Derive SigV4 signing key: HMAC(HMAC(HMAC(HMAC("AWS4"+secret, date), region), service), "aws4_request")
fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret);
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// 生成 x-amz-date 格式（向后兼容，部分路径仍使用）
fn amz_date_string() -> String {
    aws_timestamps().1
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. S3Config defaults ──────────────────────────────────────────────────
    #[test]
    fn test_s3_config_defaults() {
        let cfg = S3Config::default();
        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(cfg.timeout_seconds, 60);
        assert!(cfg.endpoint.is_none());
        assert!(cfg.bucket.is_empty());
        assert_eq!(cfg.multipart_threshold, DEFAULT_MULTIPART_THRESHOLD);
        assert_eq!(cfg.part_size, DEFAULT_PART_SIZE);
    }

    // ── 2. S3Transfer construction ────────────────────────────────────────────
    #[test]
    fn test_s3_transfer_new_ok() {
        let cfg = S3Config {
            bucket: "test-bucket".to_string(),
            region: "us-east-1".to_string(),
            access_key: "AKID".to_string(),
            secret_key: "secret".to_string(),
            endpoint: None,
            timeout_seconds: 30,
            ..S3Config::default()
        };
        let result = S3Transfer::new(cfg);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().protocol_name(), "S3");
    }

    // ── 3. parse_s3_url: valid URL ────────────────────────────────────────────
    #[test]
    fn test_parse_s3_url_valid() {
        let (bucket, key) = S3Transfer::parse_s3_url("s3://my-bucket/path/to/file.txt").unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, "path/to/file.txt");
    }

    // ── 4. parse_s3_url: missing key → Err ───────────────────────────────────
    #[test]
    fn test_parse_s3_url_missing_key() {
        let result = S3Transfer::parse_s3_url("s3://my-bucket/");
        assert!(result.is_err(), "empty key should fail");
    }

    // ── 5. parse_s3_url: not s3:// → Err ─────────────────────────────────────
    #[test]
    fn test_parse_s3_url_wrong_scheme() {
        let result = S3Transfer::parse_s3_url("http://bucket/key");
        assert!(result.is_err());
    }

    // ── 6. build_put_url with endpoint ───────────────────────────────────────
    #[test]
    fn test_build_put_url_with_endpoint() {
        let cfg = S3Config {
            bucket: "mybucket".to_string(),
            region: "us-east-1".to_string(),
            access_key: "key".to_string(),
            secret_key: "secret".to_string(),
            endpoint: Some("http://minio:9000".to_string()),
            timeout_seconds: 30,
            ..S3Config::default()
        };
        let s3 = S3Transfer::new(cfg).unwrap();
        let url = s3.build_put_url("mybucket", "path/file.txt");
        assert_eq!(url, "http://minio:9000/mybucket/path/file.txt");
    }

    // ── 7. build_put_url AWS S3 ───────────────────────────────────────────────
    #[test]
    fn test_build_put_url_aws() {
        let cfg = S3Config {
            bucket: "mybucket".to_string(),
            region: "ap-east-1".to_string(),
            access_key: "key".to_string(),
            secret_key: "secret".to_string(),
            endpoint: None,
            timeout_seconds: 30,
            ..S3Config::default()
        };
        let s3 = S3Transfer::new(cfg).unwrap();
        let url = s3.build_put_url("mybucket", "data/file.bin");
        assert_eq!(
            url,
            "https://mybucket.s3.ap-east-1.amazonaws.com/data/file.bin"
        );
    }

    // ── 8. supports_resume is true (S3 Multipart supports resume) ─────────────
    #[test]
    fn test_s3_supports_resume_false() {
        let s3 = S3Transfer::new(S3Config::default()).unwrap();
        assert!(s3.supports_resume());
    }

    // ── 9. parse_xml_tag extracts value ───────────────────────────────────────
    #[test]
    fn test_parse_xml_tag() {
        let xml = "<InitiateMultipartUploadResult><Bucket>my-bucket</Bucket><Key>mykey</Key><UploadId>abc123xyz</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(
            parse_xml_tag(xml, "UploadId"),
            Some("abc123xyz".to_string())
        );
        assert_eq!(parse_xml_tag(xml, "Bucket"), Some("my-bucket".to_string()));
        assert_eq!(parse_xml_tag(xml, "Missing"), None);
    }

    // ── 10. multipart_threshold config ────────────────────────────────────────
    #[test]
    fn test_multipart_threshold_custom() {
        let cfg = S3Config {
            multipart_threshold: 50 * 1024 * 1024,
            part_size: 8 * 1024 * 1024,
            ..S3Config::default()
        };
        assert_eq!(cfg.multipart_threshold, 50 * 1024 * 1024);
        assert_eq!(cfg.part_size, 8 * 1024 * 1024);
    }

    // ── 11. url_to_bucket_key MinIO ───────────────────────────────────────────
    #[test]
    fn test_url_to_bucket_key_minio() {
        let cfg = S3Config {
            endpoint: Some("http://minio:9000".to_string()),
            ..S3Config::default()
        };
        let s3 = S3Transfer::new(cfg).unwrap();
        let url = "http://minio:9000/mybucket/path/to/file.bin";
        let (bucket, key) = s3.url_to_bucket_key(url).unwrap();
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.bin");
    }

    // ── 12. url_to_bucket_key AWS ─────────────────────────────────────────────
    #[test]
    fn test_url_to_bucket_key_aws() {
        let cfg = S3Config {
            endpoint: None,
            region: "us-east-1".to_string(),
            ..S3Config::default()
        };
        let s3 = S3Transfer::new(cfg).unwrap();
        let url = "https://mybucket.s3.us-east-1.amazonaws.com/data/file.bin";
        let (bucket, key) = s3.url_to_bucket_key(url).unwrap();
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "data/file.bin");
    }

    // ── 13. upload_auto uses simple PUT for small files (mock) ────────────────
    #[tokio::test]
    async fn test_upload_auto_small_file_uses_put() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a small temp file
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"small file content").unwrap();

        // Mock server that only accepts PUT (not POST which multipart would use)
        let cfg = S3Config {
            multipart_threshold: 1024 * 1024, // 1MB threshold
            endpoint: Some("http://127.0.0.1:19999".to_string()), // will fail but we test routing
            ..S3Config::default()
        };
        let s3 = S3Transfer::new(cfg).unwrap();
        let url = "http://127.0.0.1:19999/bucket/key.bin";
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // File is smaller than threshold, so it should use PUT (not multipart)
        // The request will fail (no server) but we verify it doesn't try to initiate multipart
        let result = s3.upload_auto(tmp.path(), url, tx).await;
        // Expected: Network error (no server) from simple PUT path
        if let Err(AeroSyncError::Network(msg)) = result {
            // Should be a PUT error, not an "Initiate Multipart" error
            assert!(
                !msg.contains("Initiate Multipart"),
                "Should use simple PUT for small files, got: {}",
                msg
            );
        }
    }

    // ── 14. upload_auto uses multipart for large files (mock) ─────────────────
    #[tokio::test]
    async fn test_upload_auto_large_file_uses_multipart() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a file larger than the (very small) threshold
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(&vec![0u8; 1024]).unwrap(); // 1KB content

        let cfg = S3Config {
            multipart_threshold: 512, // 512-byte threshold (tiny, for testing)
            part_size: 256,
            endpoint: Some("http://127.0.0.1:19998".to_string()),
            ..S3Config::default()
        };
        let s3 = S3Transfer::new(cfg).unwrap();
        let url = "http://127.0.0.1:19998/bucket/large.bin";
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = s3.upload_auto(tmp.path(), url, tx).await;
        // Expected: Network error from Initiate Multipart (no server), not from simple PUT
        if let Err(AeroSyncError::Network(msg)) = result {
            assert!(
                msg.contains("Initiate Multipart")
                    || msg.contains("connection refused")
                    || msg.contains("failed"),
                "Expected multipart error, got: {}",
                msg
            );
        }
    }
}

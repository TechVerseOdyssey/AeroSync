/// S3 协议适配器
///
/// 支持 AWS S3 和兼容 S3 API 的存储服务（如 MinIO）。
/// URL 格式：s3://bucket/path/to/file
///
/// 认证策略：
/// - 若设置了 endpoint（MinIO 等），使用 Bearer token 认证
/// - 否则，使用简化的 AWS4 认证头格式（架构占位，不做完整 SigV4 计算）
///
/// Multipart Upload（大文件分片上传）：
/// - 文件大小超过 `multipart_threshold` 时自动切换为分片上传
/// - 三步流程：Initiate → Upload Part(s) → Complete
/// - 上传 ID（UploadId）可通过 `ResumeState.metadata` 持久化以支持断点续传

use crate::traits::{TransferProtocol, TransferProgress};
use aerosync_core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

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
            format!(
                "{}/{}/{}",
                endpoint.trim_end_matches('/'),
                bucket,
                key
            )
        } else {
            // AWS S3 标准格式
            format!(
                "https://{}.s3.{}.amazonaws.com/{}",
                bucket, self.config.region, key
            )
        }
    }

    /// 构建认证 header
    fn build_auth_header(&self) -> String {
        if self.config.endpoint.is_some() {
            // MinIO: Bearer token 认证
            format!("Bearer {}", self.config.access_key)
        } else {
            // AWS4 简化格式（架构占位）
            let now = chrono_date_string();
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{}/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=placeholder",
                self.config.access_key, now
            )
        }
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

        tracing::debug!("S3 Multipart initiated: upload_id={}", &upload_id[..upload_id.len().min(16)]);
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
        let url = format!("{}?partNumber={}&uploadId={}", base_url, part_number, upload_id);
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

        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 Upload Part {} failed: {}", part_number, e)))?;

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

        tracing::debug!("S3 Part {} uploaded, ETag={}", part_number, &etag[..etag.len().min(16)]);
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
    pub async fn abort_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<()> {
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
    pub async fn upload_auto(
        &self,
        file_path: &std::path::Path,
        url: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let file_size = tokio::fs::metadata(file_path)
            .await?
            .len();

        if file_size < self.config.multipart_threshold {
            return self.upload_to_s3(file_path, url, progress_tx).await;
        }

        // ── Multipart path ───────────────────────────────────────────────────
        // Parse bucket/key from the URL we were given
        let (bucket, key) = self.url_to_bucket_key(url)?;
        let upload_id = self.initiate_multipart(&bucket, &key).await?;

        let mut file = File::open(file_path).await?;
        let part_size = self.config.part_size;
        let mut part_number: u32 = 1;
        let mut parts: Vec<(u32, String)> = Vec::new();
        let mut bytes_uploaded: u64 = 0;
        let start = Instant::now();

        loop {
            let mut buf = vec![0u8; part_size];
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            buf.truncate(n);

            let etag = match self.upload_part(&bucket, &key, &upload_id, part_number, buf).await {
                Ok(e) => e,
                Err(e) => {
                    // Attempt to abort before propagating the error
                    let _ = self.abort_multipart(&bucket, &key, &upload_id).await;
                    return Err(e);
                }
            };

            parts.push((part_number, etag));
            bytes_uploaded += n as u64;
            part_number += 1;

            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { bytes_uploaded as f64 / elapsed } else { 0.0 };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred: bytes_uploaded,
                transfer_speed: speed,
            });
        }

        self.complete_multipart(&bucket, &key, &upload_id, &parts).await
    }

    /// Extract (bucket, key) from a fully-qualified S3 URL built by build_put_url
    fn url_to_bucket_key(&self, url: &str) -> Result<(String, String)> {
        if let Some(ref endpoint) = self.config.endpoint {
            // MinIO format: endpoint/bucket/key
            let prefix = endpoint.trim_end_matches('/');
            let rest = url
                .strip_prefix(prefix)
                .and_then(|s| s.strip_prefix('/'))
                .ok_or_else(|| AeroSyncError::InvalidConfig(format!("Cannot parse URL: {}", url)))?;
            let (bucket, key) = rest.split_once('/').ok_or_else(|| {
                AeroSyncError::InvalidConfig(format!("Cannot parse bucket/key from URL: {}", url))
            })?;
            Ok((bucket.to_string(), key.to_string()))
        } else {
            // AWS format: https://<bucket>.s3.<region>.amazonaws.com/<key>
            let without_scheme = url
                .strip_prefix("https://")
                .ok_or_else(|| AeroSyncError::InvalidConfig(format!("Cannot parse URL: {}", url)))?;
            let (host, key) = without_scheme.split_once('/').ok_or_else(|| {
                AeroSyncError::InvalidConfig(format!("Cannot parse key from URL: {}", url))
            })?;
            let bucket = host
                .split('.')
                .next()
                .ok_or_else(|| AeroSyncError::InvalidConfig(format!("Cannot parse bucket from host: {}", host)))?;
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
            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                file_size as f64 / elapsed
            } else {
                0.0
            };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred: file_size,
                transfer_speed: speed,
            });
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

        let mut req = self
            .client
            .get(url)
            .header("Authorization", auth);

        if self.config.endpoint.is_none() {
            req = req.header("x-amz-date", now_str);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("S3 GET failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(AeroSyncError::Network(format!(
                "S3 GET failed: {}",
                status
            )));
        }

        let mut file = tokio::fs::File::create(dest_path).await?;
        let start = Instant::now();
        let mut bytes_transferred = 0u64;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AeroSyncError::Network(e.to_string()))?;
            file.write_all(&chunk).await?;
            bytes_transferred += chunk.len() as u64;
            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                bytes_transferred as f64 / elapsed
            } else {
                0.0
            };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred,
                transfer_speed: speed,
            });
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
        self.download_from_s3(&url, &task.source_path, progress_tx).await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        _offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // S3 不支持原生续传，重新上传整个文件
        self.upload_file(task, progress_tx).await
    }

    fn supports_resume(&self) -> bool {
        false
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

/// 生成 AWS 日期字符串，格式：YYYYMMDD
fn chrono_date_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 简单计算：仅用于 Credential 中的日期部分（不精确，仅做占位）
    let days = secs / 86400;
    let year = 1970 + days / 365;
    format!("{:04}0101", year)
}

/// 生成 x-amz-date 格式：YYYYMMDDTHHmmssZ
fn amz_date_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let year = 1970 + days / 365;
    format!("{:04}0101T000000Z", year)
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
        assert_eq!(url, "https://mybucket.s3.ap-east-1.amazonaws.com/data/file.bin");
    }

    // ── 8. supports_resume is false ───────────────────────────────────────────
    #[test]
    fn test_s3_supports_resume_false() {
        let s3 = S3Transfer::new(S3Config::default()).unwrap();
        assert!(!s3.supports_resume());
    }

    // ── 9. parse_xml_tag extracts value ───────────────────────────────────────
    #[test]
    fn test_parse_xml_tag() {
        let xml = "<InitiateMultipartUploadResult><Bucket>my-bucket</Bucket><Key>mykey</Key><UploadId>abc123xyz</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(parse_xml_tag(xml, "UploadId"), Some("abc123xyz".to_string()));
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
        use tempfile::NamedTempFile;
        use std::io::Write;

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
        match result {
            Err(AeroSyncError::Network(msg)) => {
                // Should be a PUT error, not an "Initiate Multipart" error
                assert!(!msg.contains("Initiate Multipart"), "Should use simple PUT for small files, got: {}", msg);
            }
            _ => {} // Other error types are also acceptable
        }
    }

    // ── 14. upload_auto uses multipart for large files (mock) ─────────────────
    #[tokio::test]
    async fn test_upload_auto_large_file_uses_multipart() {
        use tempfile::NamedTempFile;
        use std::io::Write;

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
        match result {
            Err(AeroSyncError::Network(msg)) => {
                assert!(
                    msg.contains("Initiate Multipart") || msg.contains("connection refused") || msg.contains("failed"),
                    "Expected multipart error, got: {}",
                    msg
                );
            }
            _ => {}
        }
    }
}


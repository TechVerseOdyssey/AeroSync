/// S3 协议适配器
///
/// 支持 AWS S3 和兼容 S3 API 的存储服务（如 MinIO）。
/// URL 格式：s3://bucket/path/to/file
///
/// 认证策略：
/// - 若设置了 endpoint（MinIO 等），使用 Bearer token 认证
/// - 否则，使用简化的 AWS4 认证头格式（架构占位，不做完整 SigV4 计算）

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
        self.upload_to_s3(&task.source_path, &url, progress_tx).await
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
}

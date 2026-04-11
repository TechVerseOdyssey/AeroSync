/// 发送前预检验模块
///
/// 在实际传输前探测接收端状态（磁盘空间、版本兼容等），
/// 磁盘不足时提前报错，避免半途中断。
use crate::{AeroSyncError, Result};

/// 预检验结果
#[derive(Debug, Clone)]
pub struct PreflightResult {
    /// 接收端 AeroSync 版本
    pub version: Option<String>,
    /// 接收端可用磁盘空间（bytes）
    pub free_bytes: u64,
    /// 接收端总磁盘空间（bytes）
    pub total_bytes: u64,
    /// 已接收文件数
    pub received_files: u64,
}

/// 预检验错误
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("Cannot reach receiver at {url}: {reason}")]
    Unreachable { url: String, reason: String },

    #[error("Insufficient disk space on receiver: need {need} bytes, free {free} bytes")]
    InsufficientDisk { need: u64, free: u64 },

    #[error("Receiver returned error: {0}")]
    ReceiverError(String),
}

/// 探测接收端健康状态
///
/// `http_base` 示例：`http://192.168.1.10:7788`
pub async fn probe_receiver(http_base: &str) -> Result<PreflightResult> {
    let url = format!("{}/health", http_base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| AeroSyncError::Network(e.to_string()))?;

    let resp = client.get(&url).send().await.map_err(|e| {
        AeroSyncError::Network(format!("Preflight probe failed for {}: {}", url, e))
    })?;

    if !resp.status().is_success() {
        return Err(AeroSyncError::Network(format!(
            "Receiver health check returned {}",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        AeroSyncError::Network(format!("Failed to parse health response: {}", e))
    })?;

    Ok(PreflightResult {
        version: body["version"].as_str().map(|s| s.to_string()),
        free_bytes: body["free_bytes"].as_u64().unwrap_or(0),
        total_bytes: body["total_bytes"].as_u64().unwrap_or(0),
        received_files: body["received_files"].as_u64().unwrap_or(0),
    })
}

/// 预检验：确认接收端磁盘空间足够容纳 `total_bytes` 字节
///
/// `http_base` 示例：`http://192.168.1.10:7788`
/// `total_bytes`：即将发送的文件总字节数
///
/// 若空间不足，返回 Err；`free_bytes == 0` 时（接收端不支持磁盘查询）视为跳过检查。
pub async fn preflight_check(http_base: &str, total_bytes: u64) -> std::result::Result<PreflightResult, PreflightError> {
    let result = probe_receiver(http_base).await.map_err(|e| PreflightError::Unreachable {
        url: http_base.to_string(),
        reason: e.to_string(),
    })?;

    // free_bytes == 0 表示接收端未上报磁盘信息（旧版本兼容），跳过检查
    if result.free_bytes > 0 && total_bytes > 0 && result.free_bytes < total_bytes {
        return Err(PreflightError::InsufficientDisk {
            need: total_bytes,
            free: result.free_bytes,
        });
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preflight_result_fields() {
        let r = PreflightResult {
            version: Some("0.2.0".to_string()),
            free_bytes: 1024 * 1024 * 1024,
            total_bytes: 10 * 1024 * 1024 * 1024,
            received_files: 3,
        };
        assert_eq!(r.version.as_deref(), Some("0.2.0"));
        assert_eq!(r.free_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn test_preflight_insufficient_disk_error_message() {
        let err = PreflightError::InsufficientDisk { need: 1000, free: 500 };
        let msg = err.to_string();
        assert!(msg.contains("500"), "message: {}", msg);
        assert!(msg.contains("1000"), "message: {}", msg);
    }

    #[test]
    fn test_preflight_unreachable_error_message() {
        let err = PreflightError::Unreachable {
            url: "http://host:7788".to_string(),
            reason: "connection refused".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("host:7788"), "message: {}", msg);
    }

    /// 集成测试：需要真实服务，在 CI 中跳过
    #[tokio::test]
    #[ignore]
    async fn test_probe_real_receiver() {
        let result = probe_receiver("http://127.0.0.1:7788").await.unwrap();
        assert!(result.total_bytes > 0);
    }
}

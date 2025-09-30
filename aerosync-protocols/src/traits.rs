use aerosync_core::{Result, TransferTask};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProtocolConfig {
    Http {
        timeout_seconds: u64,
        max_retries: u32,
        chunk_size: usize,
    },
    Quic {
        server_name: String,
        alpn_protocols: Vec<String>,
        max_idle_timeout: u64,
        keep_alive_interval: u64,
    },
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub bytes_transferred: u64,
    pub transfer_speed: f64,
}

#[async_trait]
pub trait TransferProtocol: Send + Sync {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()>;

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()>;

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()>;

    fn supports_resume(&self) -> bool;
    fn protocol_name(&self) -> &'static str;
}
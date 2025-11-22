use crate::traits::{TransferProtocol, TransferProgress, ProtocolConfig};
use aerosync_core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

pub struct HttpTransfer {
    client: Client,
    config: HttpConfig,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub chunk_size: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 30,
            max_retries: 3,
            chunk_size: 1024 * 1024, // 1MB
        }
    }
}

impl HttpTransfer {
    pub fn new(config: HttpConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        Ok(Self { client, config })
    }

    async fn upload_with_progress(
        &self,
        file_path: &std::path::Path,
        url: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let mut file = File::open(file_path).await?;
        let file_size = file.metadata().await?.len();
        
        let start_time = Instant::now();
        let mut bytes_transferred = 0u64;

        // Get file name for multipart form
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        // Create multipart form with file
        let mut form = reqwest::multipart::Form::new();
        
        // Read file in chunks and create a stream
        let mut file_contents = Vec::new();
        file.read_to_end(&mut file_contents).await?;
        
        let file_part = reqwest::multipart::Part::bytes(file_contents)
            .file_name(file_name.clone())
            .mime_str("application/octet-stream")
            .map_err(|e| AeroSyncError::Network(format!("Failed to create multipart part: {}", e)))?;
        
        form = form.part("file", file_part);

        tracing::info!("HTTP: Uploading file '{}' to {}", file_name, url);
        
        let response = self.client
            .post(url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(format!("Upload request failed: {}", e)))?;

        if response.status().is_success() {
            bytes_transferred = file_size;
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { bytes_transferred as f64 / elapsed } else { 0.0 };

            let _ = progress_tx.send(TransferProgress {
                bytes_transferred,
                transfer_speed: speed,
            });

            tracing::info!("HTTP: Upload completed successfully: {} bytes at {:.2} MB/s", 
                         bytes_transferred, speed / (1024.0 * 1024.0));
            Ok(())
        } else {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!("HTTP: Upload failed with status {}: {}", status, error_text);
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
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let response = self.client
            .get(url)
            .send()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(AeroSyncError::Network(format!(
                "Download failed with status: {}",
                response.status()
            )));
        }

        let total_size = response.content_length().unwrap_or(0);
        let mut file = File::create(file_path).await?;
        let mut bytes_transferred = 0u64;
        let start_time = Instant::now();

        let mut stream = response.bytes_stream();
        use futures::StreamExt;

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

#[async_trait]
impl TransferProtocol for HttpTransfer {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        self.upload_with_progress(&task.source_path, &task.destination, progress_tx).await
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        self.download_with_progress(&task.destination, &task.source_path, progress_tx).await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // TODO: Implement HTTP range requests for resume functionality
        if task.is_upload {
            self.upload_file(task, progress_tx).await
        } else {
            self.download_file(task, progress_tx).await
        }
    }

    fn supports_resume(&self) -> bool {
        true // HTTP supports range requests
    }

    fn protocol_name(&self) -> &'static str {
        "HTTP"
    }
}
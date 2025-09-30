use crate::{AppState, UiEvent, UiEventHandler};
use aerosync_core::{TransferTask, FileManager, FileInfo};
use std::io::{self, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub struct CliApp {
    state: AppState,
    event_rx: mpsc::UnboundedReceiver<UiEvent>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
}

impl CliApp {
    pub fn new(state: AppState) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            state,
            event_rx,
            event_tx,
        }
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        println!("AeroSync - Cross-Platform File Transfer Engine");
        println!("==============================================");
        
        loop {
            self.show_main_menu().await?;
            
            let input = self.read_input("Select an option: ")?;
            match input.trim() {
                "1" => self.handle_file_selection().await?,
                "2" => self.handle_folder_selection().await?,
                "3" => self.show_transfer_status().await?,
                "4" => self.show_settings().await?,
                "5" => {
                    println!("Goodbye!");
                    break;
                }
                _ => println!("Invalid option. Please try again."),
            }
        }
        
        Ok(())
    }

    async fn show_main_menu(&self) -> Result<(), Box<dyn std::error::Error>> {
        println!("\nMain Menu:");
        println!("1. Select files to transfer");
        println!("2. Select folder to transfer");
        println!("3. View transfer status");
        println!("4. Settings");
        println!("5. Exit");
        Ok(())
    }

    async fn handle_file_selection(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let path_input = self.read_input("Enter file path (or multiple paths separated by ;): ")?;
        let paths: Vec<PathBuf> = path_input
            .split(';')
            .map(|s| PathBuf::from(s.trim()))
            .collect();

        println!("Selected files:");
        for path in &paths {
            if let Ok(file_info) = FileManager::get_file_info(path).await {
                println!("  {} ({} bytes)", file_info.name, file_info.size);
            } else {
                println!("  {} (error reading file)", path.display());
            }
        }

        let destination = self.read_input("Enter destination URL: ")?;
        
        // Create transfer tasks
        for path in paths {
            if let Ok(file_info) = FileManager::get_file_info(&path).await {
                let task = TransferTask::new_upload(path, destination.clone(), file_info.size);
                if let Err(e) = self.state.transfer_engine.add_transfer(task).await {
                    println!("Failed to add transfer: {}", e);
                }
            }
        }

        println!("Files queued for transfer!");
        Ok(())
    }

    async fn handle_folder_selection(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let folder_path = self.read_input("Enter folder path: ")?;
        let path = PathBuf::from(folder_path);

        match FileManager::list_directory(&path).await {
            Ok(files) => {
                println!("Files in folder:");
                let mut total_size = 0u64;
                for file in &files {
                    if !file.is_directory {
                        println!("  {} ({} bytes)", file.name, file.size);
                        total_size += file.size;
                    }
                }
                println!("Total size: {} bytes", total_size);

                let confirm = self.read_input("Transfer all files? (y/n): ")?;
                if confirm.trim().to_lowercase() == "y" {
                    let destination = self.read_input("Enter destination URL: ")?;
                    
                    for file in files {
                        if !file.is_directory {
                            let task = TransferTask::new_upload(file.path, destination.clone(), file.size);
                            if let Err(e) = self.state.transfer_engine.add_transfer(task).await {
                                println!("Failed to add transfer for {}: {}", file.name, e);
                            }
                        }
                    }
                    println!("Folder queued for transfer!");
                }
            }
            Err(e) => println!("Error reading folder: {}", e),
        }

        Ok(())
    }

    async fn show_transfer_status(&self) -> Result<(), Box<dyn std::error::Error>> {
        let monitor = self.state.transfer_engine.get_progress_monitor().await;
        let monitor_guard = monitor.read().await;
        
        let stats = monitor_guard.get_stats();
        let active_transfers = monitor_guard.get_active_transfers();

        println!("\nTransfer Status:");
        println!("===============");
        println!("Total files: {}", stats.total_files);
        println!("Completed: {}", stats.completed_files);
        println!("Failed: {}", stats.failed_files);
        println!("Overall speed: {:.2} MB/s", stats.overall_speed / (1024.0 * 1024.0));

        if !active_transfers.is_empty() {
            println!("\nActive transfers:");
            for transfer in active_transfers {
                let progress_percent = if transfer.total_bytes > 0 {
                    (transfer.bytes_transferred as f64 / transfer.total_bytes as f64) * 100.0
                } else {
                    0.0
                };
                
                println!("  {} - {:.1}% ({:.2} MB/s)", 
                    transfer.file_name,
                    progress_percent,
                    transfer.transfer_speed / (1024.0 * 1024.0)
                );
            }
        }

        Ok(())
    }

    async fn show_settings(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config = self.state.config.read().await;
        println!("\nCurrent Settings:");
        println!("================");
        println!("Max concurrent transfers: {}", config.max_concurrent_transfers);
        println!("Chunk size: {} bytes", config.chunk_size);
        println!("Retry attempts: {}", config.retry_attempts);
        println!("Timeout: {} seconds", config.timeout_seconds);
        println!("Use QUIC: {}", config.use_quic);
        Ok(())
    }

    fn read_input(&self, prompt: &str) -> Result<String, io::Error> {
        print!("{}", prompt);
        io::stdout().flush()?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok(input)
    }
}

impl UiEventHandler for CliApp {
    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::ProgressUpdate(progress) => {
                println!("Progress: {} - {}/{} bytes", 
                    progress.file_name,
                    progress.bytes_transferred,
                    progress.total_bytes
                );
            }
            UiEvent::TransferError { task_id, error } => {
                println!("Transfer error for {}: {}", task_id, error);
            }
            UiEvent::SystemError(error) => {
                println!("System error: {}", error);
            }
            _ => {} // Handle other events as needed
        }
    }
}
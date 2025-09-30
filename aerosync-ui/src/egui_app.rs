#[cfg(feature = "egui")]
use eframe::egui;
#[cfg(feature = "egui")]
use rfd::FileDialog;
use crate::AppState;
use aerosync_core::{TransferTask, FileManager, ServerStatus};
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "egui")]
pub struct EguiApp {
    state: AppState,
    selected_files: Vec<PathBuf>,
    destination_url: String,
    show_settings: bool,
    show_server_panel: bool,
    transfer_status: String,
    server_status: String,
    rt: tokio::runtime::Runtime,
    config_cache: aerosync_core::TransferConfig,
    server_config_cache: aerosync_core::ServerConfig,
}

#[cfg(feature = "egui")]
impl EguiApp {
    pub fn new(state: AppState) -> Self {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create async runtime");
        let (config_cache, server_config_cache) = rt.block_on(async {
            let config = state.config.read().await.clone();
            let server_config = state.server_config.read().await.clone();
            
            // Start the transfer engine
            if let Err(e) = state.transfer_engine.start().await {
                tracing::error!("Failed to start transfer engine: {}", e);
            }
            
            (config, server_config)
        });
        
        Self {
            state,
            selected_files: Vec::new(),
            destination_url: "http://localhost:8080/upload".to_string(),
            show_settings: false,
            show_server_panel: false,
            transfer_status: "Ready".to_string(),
            server_status: "Server stopped".to_string(),
            rt,
            config_cache,
            server_config_cache,
        }
    }

    pub fn run(state: AppState) -> Result<(), eframe::Error> {
        let options = eframe::NativeOptions {
            initial_window_size: Some(egui::vec2(900.0, 700.0)),
            min_window_size: Some(egui::vec2(600.0, 400.0)),
            ..Default::default()
        };

        eframe::run_native(
            "AeroSync - File Transfer Engine",
            options,
            Box::new(move |_cc| {
                Box::new(EguiApp::new(state))
            }),
        )
    }

    fn select_files(&mut self) {
        // Try multiple files selection with better configuration
        match FileDialog::new().pick_files()
        {
            Some(files) if !files.is_empty() => {
                self.selected_files = files;
                self.transfer_status = format!("Selected {} files", self.selected_files.len());
            }
            _ => {
                // Fallback to single file selection if multiple selection fails or is empty
                if let Some(file) = FileDialog::new()
                    .pick_file()
                {
                    self.selected_files = vec![file];
                    self.transfer_status = "Selected 1 file".to_string();
                } else {
                    self.transfer_status = "No files selected".to_string();
                }
            }
        }
    }

    fn select_single_file(&mut self) {
        if let Some(file) = FileDialog::new().pick_file() {
            self.selected_files = vec![file];
            self.transfer_status = "Selected 1 file".to_string();
        } else {
            self.transfer_status = "No file selected".to_string();
        }
    }

    fn select_folder(&mut self) {
        if let Some(folder) = FileDialog::new()
            .set_directory(".")
            .pick_folder()
        {
            // Get all files in the folder
            let files = self.rt.block_on(async {
                match FileManager::list_directory(&folder).await {
                    Ok(file_infos) => {
                        file_infos.into_iter()
                            .filter(|f| !f.is_directory)
                            .map(|f| f.path)
                            .collect::<Vec<_>>()
                    }
                    Err(_) => Vec::new()
                }
            });
            
            self.selected_files = files;
            self.transfer_status = format!("Selected folder with {} files", self.selected_files.len());
        }
    }

    fn start_transfer(&mut self) {
        if self.selected_files.is_empty() {
            self.transfer_status = "No files selected!".to_string();
            return;
        }

        if self.destination_url.is_empty() {
            self.transfer_status = "No destination URL specified!".to_string();
            return;
        }

        let files = self.selected_files.clone();
        let destination = self.destination_url.clone();
        let engine = Arc::clone(&self.state.transfer_engine);

        self.rt.spawn(async move {
            for file_path in files {
                if let Ok(file_info) = FileManager::get_file_info(&file_path).await {
                    let task = TransferTask::new_upload(
                        file_path,
                        destination.clone(),
                        file_info.size
                    );
                    
                    if let Err(e) = engine.add_transfer(task).await {
                        tracing::error!("Failed to add transfer: {}", e);
                    }
                }
            }
        });

        self.transfer_status = format!("Started transfer of {} files", self.selected_files.len());
    }

    fn get_transfer_stats(&self) -> (usize, usize, usize, f64) {
        self.rt.block_on(async {
            let monitor = self.state.transfer_engine.get_progress_monitor().await;
            let monitor_guard = monitor.read().await;
            let stats = monitor_guard.get_stats();
            (
                stats.total_files,
                stats.completed_files,
                stats.failed_files,
                stats.overall_speed / (1024.0 * 1024.0) // Convert to MB/s
            )
        })
    }

    fn get_active_transfers(&self) -> Vec<(String, f64, f64)> {
        self.rt.block_on(async {
            let monitor = self.state.transfer_engine.get_progress_monitor().await;
            let monitor_guard = monitor.read().await;
            let transfers = monitor_guard.get_active_transfers();
            
            transfers.iter().map(|t| {
                let progress = if t.total_bytes > 0 {
                    (t.bytes_transferred as f64 / t.total_bytes as f64) * 100.0
                } else {
                    0.0
                };
                let speed_mb = t.transfer_speed / (1024.0 * 1024.0);
                (t.file_name.clone(), progress, speed_mb)
            }).collect()
        })
    }

    fn start_server(&mut self) {
        let receiver = Arc::clone(&self.state.file_receiver);
        self.rt.spawn(async move {
            let mut receiver_guard = receiver.write().await;
            if let Err(e) = receiver_guard.start().await {
                tracing::error!("Failed to start server: {}", e);
            }
        });
        self.server_status = "Starting server...".to_string();
    }

    fn stop_server(&mut self) {
        let receiver = Arc::clone(&self.state.file_receiver);
        self.rt.spawn(async move {
            let mut receiver_guard = receiver.write().await;
            if let Err(e) = receiver_guard.stop().await {
                tracing::error!("Failed to stop server: {}", e);
            }
        });
        self.server_status = "Stopping server...".to_string();
    }

    fn get_server_status(&self) -> (String, Vec<String>) {
        self.rt.block_on(async {
            let receiver = self.state.file_receiver.read().await;
            let status = receiver.get_status().await;
            let urls = receiver.get_server_urls().await;
            
            let status_text = match status {
                ServerStatus::Stopped => "Stopped".to_string(),
                ServerStatus::Starting => "Starting...".to_string(),
                ServerStatus::Running => "Running".to_string(),
                ServerStatus::Error(e) => format!("Error: {}", e),
            };
            
            (status_text, urls)
        })
    }

    fn get_received_files(&self) -> Vec<(String, u64, String)> {
        self.rt.block_on(async {
            let receiver = self.state.file_receiver.read().await;
            let files = receiver.get_received_files().await;
            
            files.iter().map(|f| {
                let time_str = f.received_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| {
                        let secs = d.as_secs();
                        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
                    })
                    .unwrap_or_else(|_| "Unknown".to_string());
                
                (f.original_name.clone(), f.size, time_str)
            }).collect()
        })
    }

    fn select_receive_directory(&mut self) {
        if let Some(folder) = rfd::FileDialog::new()
            .set_directory(".")
            .pick_folder()
        {
            self.server_config_cache.receive_directory = folder.clone();
            
            // Ensure the directory exists
            self.rt.spawn(async move {
                if let Err(e) = tokio::fs::create_dir_all(&folder).await {
                    tracing::error!("Failed to create receive directory: {}", e);
                }
            });
            
            self.server_status = "Receive directory updated".to_string();
        }
    }
}

#[cfg(feature = "egui")]
impl eframe::App for EguiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Request repaint for real-time updates
        ctx.request_repaint();

        egui::CentralPanel::default().show(ctx, |ui| {
            // Header
            ui.horizontal(|ui| {
                ui.heading("🚀 AeroSync");
                ui.separator();
                ui.label("Cross-Platform File Transfer Engine");
            });
            
            ui.separator();
            
            // File selection section
            ui.group(|ui| {
                ui.label("📁 File Selection");
                ui.horizontal(|ui| {
                    if ui.button("📄 Select File").clicked() {
                        self.select_single_file();
                    }
                    
                    if ui.button("📄📄 Select Files").clicked() {
                        self.select_files();
                    }
                    
                    if ui.button("📂 Select Folder").clicked() {
                        self.select_folder();
                    }
                    
                    if ui.button("🗑 Clear Selection").clicked() {
                        self.selected_files.clear();
                        self.transfer_status = "Selection cleared".to_string();
                    }
                });

                // Display selected files
                if !self.selected_files.is_empty() {
                    ui.separator();
                    ui.label(format!("Selected {} files:", self.selected_files.len()));
                    
                    egui::ScrollArea::vertical()
                        .max_height(150.0)
                        .show(ui, |ui| {
                            for (i, file) in self.selected_files.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(format!("{}.", i + 1));
                                    ui.label(file.file_name().unwrap_or_default().to_string_lossy());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(format!("({})", file.parent().unwrap_or_else(|| std::path::Path::new("")).display()));
                                    });
                                });
                            }
                        });
                }
            });

            ui.separator();

            // Destination section
            ui.group(|ui| {
                ui.label("🌐 Destination");
                ui.horizontal(|ui| {
                    ui.label("URL:");
                    ui.text_edit_singleline(&mut self.destination_url);
                });
                
                ui.horizontal(|ui| {
                    ui.label("Protocol:");
                    ui.label(if self.config_cache.use_quic { "QUIC" } else { "HTTP" });
                    ui.separator();
                    ui.label("Chunk Size:");
                    ui.label(format!("{} KB", self.config_cache.chunk_size / 1024));
                });
            });

            ui.separator();

            // Transfer controls
            ui.group(|ui| {
                ui.label("🎮 Transfer Controls");
                ui.horizontal(|ui| {
                    let start_enabled = !self.selected_files.is_empty() && !self.destination_url.is_empty();
                    
                    if ui.add_enabled(start_enabled, egui::Button::new("▶ Start Transfer")).clicked() {
                        self.start_transfer();
                    }
                    
                    if ui.button("⏸ Pause All").clicked() {
                        self.transfer_status = "Pause functionality not yet implemented".to_string();
                    }
                    
                    if ui.button("⏹ Cancel All").clicked() {
                        self.transfer_status = "Cancel functionality not yet implemented".to_string();
                    }
                    
                    ui.separator();
                    
                    if ui.button("⚙ Settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    
                    if ui.button("🖥 Server").clicked() {
                        self.show_server_panel = !self.show_server_panel;
                    }
                });
            });

            ui.separator();

            // Status and Progress
            ui.group(|ui| {
                ui.label("📊 Transfer Status");
                
                // Status message
                ui.horizontal(|ui| {
                    ui.label("Transfer:");
                    ui.colored_label(egui::Color32::from_rgb(0, 150, 0), &self.transfer_status);
                });
                
                ui.horizontal(|ui| {
                    ui.label("Server:");
                    ui.colored_label(egui::Color32::from_rgb(0, 100, 200), &self.server_status);
                });

                // Overall statistics
                let (total, completed, failed, speed) = self.get_transfer_stats();
                if total > 0 {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(format!("📈 Total: {}", total));
                        ui.separator();
                        ui.label(format!("✅ Completed: {}", completed));
                        ui.separator();
                        ui.label(format!("❌ Failed: {}", failed));
                        ui.separator();
                        ui.label(format!("🚀 Speed: {:.2} MB/s", speed));
                    });

                    // Overall progress bar
                    let overall_progress = if total > 0 {
                        (completed as f32) / (total as f32)
                    } else {
                        0.0
                    };
                    
                    ui.add(egui::ProgressBar::new(overall_progress)
                        .text(format!("{}/{} files", completed, total)));
                }

                // Active transfers
                let active_transfers = self.get_active_transfers();
                if !active_transfers.is_empty() {
                    ui.separator();
                    ui.label("🔄 Active Transfers:");
                    
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for (filename, progress, speed) in active_transfers {
                                ui.horizontal(|ui| {
                                    ui.label(&filename);
                                ui.add(egui::ProgressBar::new((progress / 100.0) as f32)
                                    .text(format!("{:.1}%", progress)));
                                    ui.label(format!("{:.2} MB/s", speed));
                                });
                            }
                        });
                }
            });
        });

        // Settings window
        if self.show_settings {
            let mut close_settings = false;
            egui::Window::new("⚙ Settings")
                .open(&mut self.show_settings)
                .default_size(egui::vec2(400.0, 300.0))
                .show(ctx, |ui| {
                    ui.group(|ui| {
                        ui.label("🔧 Transfer Configuration");
                        
                        ui.horizontal(|ui| {
                            ui.label("Max concurrent transfers:");
                            ui.add(egui::DragValue::new(&mut self.config_cache.max_concurrent_transfers)
                                .clamp_range(1..=10));
                        });
                        
                        ui.horizontal(|ui| {
                            ui.label("Chunk size (KB):");
                            let mut chunk_kb = self.config_cache.chunk_size / 1024;
                            ui.add(egui::DragValue::new(&mut chunk_kb)
                                .clamp_range(64..=8192));
                            self.config_cache.chunk_size = chunk_kb * 1024;
                        });
                        
                        ui.horizontal(|ui| {
                            ui.label("Retry attempts:");
                            ui.add(egui::DragValue::new(&mut self.config_cache.retry_attempts)
                                .clamp_range(0..=10));
                        });
                        
                        ui.horizontal(|ui| {
                            ui.label("Timeout (seconds):");
                            ui.add(egui::DragValue::new(&mut self.config_cache.timeout_seconds)
                                .clamp_range(5..=300));
                        });
                        
                        ui.checkbox(&mut self.config_cache.use_quic, "Use QUIC protocol");
                    });
                    
                    ui.separator();
                    
                    ui.group(|ui| {
                        ui.label("ℹ Information");
                        ui.label("• QUIC provides better performance over high-latency networks");
                        ui.label("• HTTP is more compatible with existing infrastructure");
                        ui.label("• Larger chunk sizes may improve throughput for large files");
                    });
                    
                    ui.separator();
                    
                    if ui.button("💾 Save Settings").clicked() {
                        let config = self.config_cache.clone();
                        let state_config = Arc::clone(&self.state.config);
                        self.rt.spawn(async move {
                            let mut config_guard = state_config.write().await;
                            *config_guard = config;
                        });
                        self.transfer_status = "Settings saved".to_string();
                        close_settings = true;
                    }
                });
            
            if close_settings {
                self.show_settings = false;
            }
        }

        // Server panel window
        let mut show_server_panel = self.show_server_panel;
        if show_server_panel {
            let mut close_server_panel = false;
            egui::Window::new("🖥 File Receiver Server")
                .open(&mut show_server_panel)
                .default_size(egui::vec2(500.0, 400.0))
                .show(ctx, |ui| {
                    let (status, urls) = self.get_server_status();
                    
                    ui.group(|ui| {
                        ui.label("📡 Server Status");
                        
                        ui.horizontal(|ui| {
                            ui.label("Status:");
                            let color = match status.as_str() {
                                "Running" => egui::Color32::from_rgb(0, 150, 0),
                                "Stopped" => egui::Color32::from_rgb(150, 150, 150),
                                _ if status.starts_with("Error") => egui::Color32::from_rgb(200, 0, 0),
                                _ => egui::Color32::from_rgb(200, 150, 0),
                            };
                            ui.colored_label(color, &status);
                        });
                        
                        if !urls.is_empty() {
                            ui.separator();
                            ui.label("📍 Server URLs:");
                            for url in &urls {
                                ui.horizontal(|ui| {
                                    ui.label("  •");
                                    let _ = ui.selectable_label(false, url);
                                    if ui.small_button("📋").clicked() {
                                        ui.output_mut(|o| o.copied_text = url.clone());
                                        self.server_status = "URL copied to clipboard".to_string();
                                    }
                                });
                            }
                        }
                    });
                    
                    ui.separator();
                    
                    ui.group(|ui| {
                        ui.label("🎮 Server Controls");
                        
                        ui.horizontal(|ui| {
                            let is_running = status == "Running";
                            
                            if ui.add_enabled(!is_running, egui::Button::new("▶ Start Server")).clicked() {
                                self.start_server();
                            }
                            
                            if ui.add_enabled(is_running, egui::Button::new("⏹ Stop Server")).clicked() {
                                self.stop_server();
                            }
                        });
                    });
                    
                    ui.separator();
                    
                    ui.group(|ui| {
                        ui.label("📁 Server Configuration");
                        
                        ui.horizontal(|ui| {
                            ui.label("HTTP Port:");
                            ui.add(egui::DragValue::new(&mut self.server_config_cache.http_port)
                                .clamp_range(1024..=65535));
                        });
                        
                        ui.horizontal(|ui| {
                            ui.label("QUIC Port:");
                            ui.add(egui::DragValue::new(&mut self.server_config_cache.quic_port)
                                .clamp_range(1024..=65535));
                        });
                        
                        ui.horizontal(|ui| {
                            ui.label("Receive Directory:");
                            ui.label(self.server_config_cache.receive_directory.display().to_string());
                            if ui.button("📂 Browse").clicked() {
                                self.select_receive_directory();
                            }
                        });
                        
                        ui.horizontal(|ui| {
                            ui.label("Max File Size (MB):");
                            let mut max_size_mb = self.server_config_cache.max_file_size / (1024 * 1024);
                            ui.add(egui::DragValue::new(&mut max_size_mb)
                                .clamp_range(1..=10240));
                            self.server_config_cache.max_file_size = max_size_mb * 1024 * 1024;
                        });
                        
                        ui.checkbox(&mut self.server_config_cache.allow_overwrite, "Allow file overwrite");
                        ui.checkbox(&mut self.server_config_cache.enable_http, "Enable HTTP server");
                        ui.checkbox(&mut self.server_config_cache.enable_quic, "Enable QUIC server");
                    });
                    
                    ui.separator();
                    
                    ui.group(|ui| {
                        ui.label("📥 Received Files");
                        
                        let received_files = self.get_received_files();
                        if received_files.is_empty() {
                            ui.label("No files received yet");
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(150.0)
                                .show(ui, |ui| {
                                    for (filename, size, time) in received_files {
                                        ui.horizontal(|ui| {
                                            ui.label(&filename);
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                ui.label(&time);
                                                ui.label(format!("{:.1} KB", size as f64 / 1024.0));
                                            });
                                        });
                                    }
                                });
                        }
                    });
                    
                    ui.separator();
                    
                    if ui.button("💾 Save Server Config").clicked() {
                        let config = self.server_config_cache.clone();
                        let state_config = Arc::clone(&self.state.server_config);
                        let receiver = Arc::clone(&self.state.file_receiver);
                        
                        self.rt.spawn(async move {
                            let mut config_guard = state_config.write().await;
                            *config_guard = config.clone();
                            
                            let mut receiver_guard = receiver.write().await;
                            if let Err(e) = receiver_guard.update_config_and_restart(config).await {
                                tracing::error!("Failed to update server config: {}", e);
                            }
                        });
                        
                        self.server_status = "Server configuration saved and applied".to_string();
                        close_server_panel = true;
                    }
                });
            
            if close_server_panel {
                show_server_panel = false;
            }
        }
        self.show_server_panel = show_server_panel;
    }
}
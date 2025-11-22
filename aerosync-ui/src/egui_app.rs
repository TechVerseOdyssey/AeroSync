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
    current_tab: Tab,
}

#[cfg(feature = "egui")]
#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Transfer,
    Server,
    History,
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
            current_tab: Tab::Transfer,
        }
    }

    pub fn run(state: AppState) -> Result<(), eframe::Error> {
        let options = eframe::NativeOptions {
            initial_window_size: Some(egui::vec2(1200.0, 800.0)),
            min_window_size: Some(egui::vec2(800.0, 600.0)),
            ..Default::default()
        };

        eframe::run_native(
            "AeroSync - File Transfer Engine",
            options,
            Box::new(move |cc| {
                // Set visual style
                let mut style = (*cc.egui_ctx.style()).clone();
                style.spacing.item_spacing = egui::vec2(8.0, 6.0);
                style.spacing.window_margin = egui::Margin::same(12.0);
                style.spacing.button_padding = egui::vec2(12.0, 6.0);
                cc.egui_ctx.set_style(style);
                
                Box::new(EguiApp::new(state))
            }),
        )
    }

    fn format_file_size(&self, bytes: u64) -> String {
        if bytes < 1024 {
            format!("{} B", bytes)
        } else if bytes < 1024 * 1024 {
            format!("{:.2} KB", bytes as f64 / 1024.0)
        } else if bytes < 1024 * 1024 * 1024 {
            format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }

    fn get_file_info(&self, path: &PathBuf) -> Option<(String, u64)> {
        self.rt.block_on(async {
            match FileManager::get_file_info(path).await {
                Ok(info) => Some((info.name, info.size)),
                Err(_) => None,
            }
        })
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

    fn get_active_transfers(&self) -> Vec<(String, f64, f64, u64, u64)> {
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
                (t.file_name.clone(), progress, speed_mb, t.bytes_transferred, t.total_bytes)
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

        // Top bar with tabs
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("🚀 AeroSync").size(24.0));
                ui.separator();
                ui.label(egui::RichText::new("Cross-Platform File Transfer Engine").color(egui::Color32::GRAY));
                
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙️").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                });
            });
            
            ui.separator();
            
            // Tab bar
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.current_tab, Tab::Transfer, "📤 Transfer");
                ui.selectable_value(&mut self.current_tab, Tab::Server, "🖥️ Server");
                ui.selectable_value(&mut self.current_tab, Tab::History, "📋 History");
            });
        });

        // Main content area
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.current_tab {
                Tab::Transfer => self.render_transfer_tab(ui),
                Tab::Server => self.render_server_tab(ui),
                Tab::History => self.render_history_tab(ui),
            }
        });

        // Settings window
        if self.show_settings {
            self.render_settings_window(ctx);
        }
    }
}

#[cfg(feature = "egui")]
impl EguiApp {
    fn render_transfer_tab(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            // File selection card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("📁 Files to Transfer");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("🗑️ Clear").clicked() {
                                self.selected_files.clear();
                                self.transfer_status = "Selection cleared".to_string();
                            }
                        });
                    });
                    
                    ui.add_space(8.0);
                    
                    // File selection buttons
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
                    });
                    
                    ui.add_space(12.0);
                    
                    // File list table
                    if !self.selected_files.is_empty() {
                        egui::Frame::none()
                            .fill(ui.style().visuals.extreme_bg_color)
                            .inner_margin(egui::Margin::same(8.0))
                            .show(ui, |ui| {
                                egui::Grid::new("file_list")
                                    .num_columns(3)
                                    .spacing([8.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.strong("File Name");
                                        ui.strong("Size");
                                        ui.strong("Path");
                                        ui.end_row();
                                        
                                        for file in &self.selected_files {
                                            // Better handling for file names with non-ASCII characters
                                            let file_name = file.file_name()
                                                .and_then(|os_str| os_str.to_str())
                                                .map(|s| s.to_string())
                                                .unwrap_or_else(|| {
                                                    file.file_name()
                                                        .unwrap_or_default()
                                                        .to_string_lossy()
                                                        .to_string()
                                                });
                                            
                                            let (name, size) = self.get_file_info(file)
                                                .unwrap_or_else(|| (file_name.clone(), 0));
                                            
                                            ui.label(&name);
                                            ui.label(self.format_file_size(size));
                                            
                                            // Better handling for path display with non-ASCII characters
                                            let path_str = file.parent()
                                                .and_then(|p| p.as_os_str().to_str())
                                                .map(|s| s.to_string())
                                                .unwrap_or_else(|| {
                                                    // Use to_string_lossy as fallback to handle non-UTF-8 paths
                                                    file.parent()
                                                        .unwrap_or_else(|| std::path::Path::new(""))
                                                        .to_string_lossy()
                                                        .to_string()
                                                });
                                            ui.label(&path_str);
                                            ui.end_row();
                                        }
                                    });
                            });
                        
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(format!("{} file(s) selected", self.selected_files.len()))
                            .color(egui::Color32::GRAY));
                    } else {
                        ui.label(egui::RichText::new("No files selected. Click buttons above to select files.")
                            .color(egui::Color32::GRAY)
                            .italics());
                    }
                });
            
            ui.add_space(16.0);
            
            // Destination card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.heading("🌐 Destination");
                    ui.add_space(8.0);
                    
                    ui.horizontal(|ui| {
                        ui.label("URL:");
                        ui.text_edit_singleline(&mut self.destination_url);
                    });
                    
                    ui.add_space(8.0);
                    
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Protocol:").strong());
                        let protocol_text = if self.config_cache.use_quic { "QUIC" } else { "HTTP" };
                        let protocol_color = if self.config_cache.use_quic {
                            egui::Color32::from_rgb(100, 200, 100)
                        } else {
                            egui::Color32::from_rgb(100, 150, 255)
                        };
                        ui.colored_label(protocol_color, protocol_text);
                        
                        ui.separator();
                        
                        ui.label(egui::RichText::new("Chunk Size:").strong());
                        ui.label(format!("{} KB", self.config_cache.chunk_size / 1024));
                    });
                });
            
            ui.add_space(16.0);
            
            // Transfer controls card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let start_enabled = !self.selected_files.is_empty() && !self.destination_url.is_empty();
                        
                        let start_button = egui::Button::new(egui::RichText::new("▶ Start Transfer").size(16.0))
                            .fill(if start_enabled {
                                egui::Color32::from_rgb(0, 150, 0)
                            } else {
                                egui::Color32::GRAY
                            });
                        
                        if ui.add_enabled(start_enabled, start_button).clicked() {
                            self.start_transfer();
                        }
                        
                        if ui.button("⏸ Pause All").clicked() {
                            self.transfer_status = "Pause functionality not yet implemented".to_string();
                        }
                        
                        if ui.button("⏹ Cancel All").clicked() {
                            self.transfer_status = "Cancel functionality not yet implemented".to_string();
                        }
                    });
                });
            
            ui.add_space(16.0);
            
            // Status and progress card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.heading("📊 Transfer Status");
                    ui.add_space(8.0);
                    
                    // Status indicators
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Status:").strong());
                        let status_color = if self.transfer_status.contains("Ready") || 
                                           self.transfer_status.contains("Started") {
                            egui::Color32::from_rgb(0, 200, 0)
                        } else if self.transfer_status.contains("Error") {
                            egui::Color32::from_rgb(200, 0, 0)
                        } else {
                            egui::Color32::from_rgb(200, 150, 0)
                        };
                        ui.colored_label(status_color, &self.transfer_status);
                    });
                    
                    ui.add_space(12.0);
                    
                    // Overall statistics
                    let (total, completed, failed, speed) = self.get_transfer_stats();
                    if total > 0 {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("📈 Total: {}", total)).strong());
                            ui.separator();
                            ui.colored_label(egui::Color32::from_rgb(0, 200, 0), 
                                format!("✅ Completed: {}", completed));
                            ui.separator();
                            ui.colored_label(egui::Color32::from_rgb(200, 0, 0), 
                                format!("❌ Failed: {}", failed));
                            ui.separator();
                            ui.colored_label(egui::Color32::from_rgb(100, 150, 255), 
                                format!("🚀 Speed: {:.2} MB/s", speed));
                        });
                        
                        ui.add_space(8.0);
                        
                        // Overall progress bar
                        let overall_progress = if total > 0 {
                            (completed as f32) / (total as f32)
                        } else {
                            0.0
                        };
                        
                        ui.add(egui::ProgressBar::new(overall_progress)
                            .fill(egui::Color32::from_rgb(0, 200, 0))
                            .text(format!("{}/{} files", completed, total)));
                    }
                    
                    ui.add_space(12.0);
                    
                    // Active transfers table
                    let active_transfers = self.get_active_transfers();
                    if !active_transfers.is_empty() {
                        ui.label(egui::RichText::new("🔄 Active Transfers").strong());
                        ui.add_space(8.0);
                        
                        egui::Frame::none()
                            .fill(ui.style().visuals.extreme_bg_color)
                            .inner_margin(egui::Margin::same(8.0))
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(250.0)
                                    .show(ui, |ui| {
                                        for (filename, progress, speed, transferred, total_bytes) in active_transfers {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&filename));
                                                
                                                ui.add(egui::ProgressBar::new((progress / 100.0) as f32)
                                                    .fill(egui::Color32::from_rgb(0, 150, 255))
                                                    .desired_width(200.0)
                                                    .text(format!("{:.1}%", progress)));
                                                
                                                ui.label(format!("{:.2} MB/s", speed));
                                                
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    ui.label(egui::RichText::new(
                                                        format!("{}/{}", 
                                                            self.format_file_size(transferred),
                                                            self.format_file_size(total_bytes)
                                                        )
                                                    ).small().color(egui::Color32::GRAY));
                                                });
                                            });
                                            ui.add_space(4.0);
                                        }
                                    });
                            });
                    }
                });
        });
    }
    
    fn render_server_tab(&mut self, ui: &mut egui::Ui) {
        let (status, urls) = self.get_server_status();
        
        ui.vertical(|ui| {
            // Server status card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("📡 Server Status");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let status_color = match status.as_str() {
                                "Running" => egui::Color32::from_rgb(0, 200, 0),
                                "Stopped" => egui::Color32::GRAY,
                                _ if status.starts_with("Error") => egui::Color32::from_rgb(200, 0, 0),
                                _ => egui::Color32::from_rgb(200, 150, 0),
                            };
                            
                            // Status indicator dot
                            ui.painter().circle_filled(
                                ui.max_rect().right_top() + egui::vec2(-8.0, 8.0),
                                6.0,
                                status_color
                            );
                            
                            ui.colored_label(status_color, &status);
                        });
                    });
                    
                    ui.add_space(12.0);
                    
                    if !urls.is_empty() {
                        ui.label(egui::RichText::new("📍 Server URLs").strong());
                        ui.add_space(8.0);
                        
                        for url in &urls {
                            ui.horizontal(|ui| {
                                ui.label("•");
                                ui.label(egui::RichText::new(url).monospace());
                                if ui.small_button("📋 Copy").clicked() {
                                    ui.output_mut(|o| o.copied_text = url.clone());
                                    self.server_status = "URL copied to clipboard".to_string();
                                }
                            });
                        }
                    }
                });
            
            ui.add_space(16.0);
            
            // Server controls card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.heading("🎮 Server Controls");
                    ui.add_space(12.0);
                    
                    let is_running = status == "Running";
                    
                    ui.horizontal(|ui| {
                        let start_color = if is_running {
                            egui::Color32::GRAY
                        } else {
                            egui::Color32::from_rgb(0, 150, 0)
                        };
                        
                        if ui.add_enabled(!is_running, 
                            egui::Button::new(egui::RichText::new("▶ Start Server").size(16.0))
                                .fill(start_color)
                        ).clicked() {
                            self.start_server();
                        }
                        
                        let stop_color = if is_running {
                            egui::Color32::from_rgb(200, 0, 0)
                        } else {
                            egui::Color32::GRAY
                        };
                        
                        if ui.add_enabled(is_running,
                            egui::Button::new(egui::RichText::new("⏹ Stop Server").size(16.0))
                                .fill(stop_color)
                        ).clicked() {
                            self.stop_server();
                        }
                    });
                });
            
            ui.add_space(16.0);
            
            // Server configuration card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.heading("⚙️ Server Configuration");
                    ui.add_space(12.0);
                    
                    ui.horizontal(|ui| {
                        ui.label("HTTP Port:");
                        ui.add(egui::DragValue::new(&mut self.server_config_cache.http_port)
                            .clamp_range(1024..=65535)
                            .speed(1.0));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("QUIC Port:");
                        ui.add(egui::DragValue::new(&mut self.server_config_cache.quic_port)
                            .clamp_range(1024..=65535)
                            .speed(1.0));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Receive Directory:");
                        let dir_str = self.server_config_cache.receive_directory
                            .as_os_str()
                            .to_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                // Use to_string_lossy as fallback to handle non-UTF-8 paths
                                self.server_config_cache.receive_directory
                                    .to_string_lossy()
                                    .to_string()
                            });
                        ui.label(egui::RichText::new(dir_str));
                        if ui.button("📂 Browse").clicked() {
                            self.select_receive_directory();
                        }
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Max File Size (MB):");
                        let mut max_size_mb = self.server_config_cache.max_file_size / (1024 * 1024);
                        ui.add(egui::DragValue::new(&mut max_size_mb)
                            .clamp_range(1..=10240)
                            .speed(10.0));
                        self.server_config_cache.max_file_size = max_size_mb * 1024 * 1024;
                    });
                    
                    ui.checkbox(&mut self.server_config_cache.allow_overwrite, "Allow file overwrite");
                    ui.checkbox(&mut self.server_config_cache.enable_http, "Enable HTTP server");
                    ui.checkbox(&mut self.server_config_cache.enable_quic, "Enable QUIC server");
                    
                    ui.add_space(12.0);
                    
                    if ui.button(egui::RichText::new("💾 Save Configuration").size(14.0)).clicked() {
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
                    }
                });
            
            ui.add_space(16.0);
            
            // Received files card
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(16.0))
                .show(ui, |ui| {
                    ui.heading("📥 Received Files");
                    ui.add_space(12.0);
                    
                    let received_files = self.get_received_files();
                    if received_files.is_empty() {
                        ui.label(egui::RichText::new("No files received yet")
                            .color(egui::Color32::GRAY)
                            .italics());
                    } else {
                        egui::Frame::none()
                            .fill(ui.style().visuals.extreme_bg_color)
                            .inner_margin(egui::Margin::same(8.0))
                            .show(ui, |ui| {
                                egui::Grid::new("received_files")
                                    .num_columns(3)
                                    .spacing([8.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.strong("File Name");
                                        ui.strong("Size");
                                        ui.strong("Received At");
                                        ui.end_row();
                                        
                                        for (filename, size, time) in received_files {
                                            ui.label(&filename);
                                            ui.label(self.format_file_size(size));
                                            ui.label(&time);
                                            ui.end_row();
                                        }
                                    });
                            });
                    }
                });
        });
    }
    
    fn render_history_tab(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(100.0);
            ui.heading("📋 Transfer History");
            ui.add_space(16.0);
            ui.label(egui::RichText::new("History feature coming soon...")
                .color(egui::Color32::GRAY)
                .italics());
        });
    }
    
    fn render_settings_window(&mut self, ctx: &egui::Context) {
        let mut close_settings = false;
        egui::Window::new("⚙️ Settings")
            .open(&mut self.show_settings)
            .default_size(egui::vec2(450.0, 400.0))
            .collapsible(false)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.heading("🔧 Transfer Configuration");
                    ui.add_space(12.0);
                    
                    ui.horizontal(|ui| {
                        ui.label("Max concurrent transfers:");
                        ui.add(egui::DragValue::new(&mut self.config_cache.max_concurrent_transfers)
                            .clamp_range(1..=10)
                            .speed(1.0));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Chunk size (KB):");
                        let mut chunk_kb = self.config_cache.chunk_size / 1024;
                        ui.add(egui::DragValue::new(&mut chunk_kb)
                            .clamp_range(64..=8192)
                            .speed(64.0));
                        self.config_cache.chunk_size = chunk_kb * 1024;
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Retry attempts:");
                        ui.add(egui::DragValue::new(&mut self.config_cache.retry_attempts)
                            .clamp_range(0..=10)
                            .speed(1.0));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Timeout (seconds):");
                        ui.add(egui::DragValue::new(&mut self.config_cache.timeout_seconds)
                            .clamp_range(5..=300)
                            .speed(5.0));
                    });
                    
                    ui.checkbox(&mut self.config_cache.use_quic, "Use QUIC protocol");
                    
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);
                    
                    ui.label(egui::RichText::new("ℹ️ Information").strong());
                    ui.add_space(8.0);
                    ui.label("• QUIC provides better performance over high-latency networks");
                    ui.label("• HTTP is more compatible with existing infrastructure");
                    ui.label("• Larger chunk sizes may improve throughput for large files");
                    
                    ui.add_space(16.0);
                    
                    if ui.button(egui::RichText::new("💾 Save Settings").size(14.0)).clicked() {
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
            });
        
        if close_settings {
            self.show_settings = false;
        }
    }
}

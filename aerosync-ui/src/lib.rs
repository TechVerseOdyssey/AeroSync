pub mod cli;
pub mod events;

#[cfg(feature = "egui")]
pub mod egui_app;

#[cfg(feature = "tauri")]
pub mod tauri_app;

pub use events::{UiEvent, UiEventHandler};

#[cfg(feature = "cli")]
pub use cli::CliApp;

#[cfg(feature = "egui")]
pub use egui_app::EguiApp;

use aerosync_core::{TransferEngine, TransferConfig, FileReceiver, ServerConfig};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AppState {
    pub transfer_engine: Arc<TransferEngine>,
    pub config: Arc<RwLock<TransferConfig>>,
    pub file_receiver: Arc<RwLock<FileReceiver>>,
    pub server_config: Arc<RwLock<ServerConfig>>,
}

impl AppState {
    pub fn new(config: TransferConfig) -> Self {
        let transfer_engine = Arc::new(TransferEngine::new(config.clone()));
        let server_config = ServerConfig::default();
        let file_receiver = FileReceiver::new(server_config.clone());
        
        Self {
            transfer_engine,
            config: Arc::new(RwLock::new(config)),
            file_receiver: Arc::new(RwLock::new(file_receiver)),
            server_config: Arc::new(RwLock::new(server_config)),
        }
    }
}
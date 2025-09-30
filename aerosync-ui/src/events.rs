use aerosync_core::{TransferTask, TransferProgress, TransferStats};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiEvent {
    // File selection events
    FilesSelected(Vec<PathBuf>),
    FolderSelected(PathBuf),
    DestinationSet(String),
    
    // Transfer control events
    StartTransfer,
    PauseTransfer(Uuid),
    ResumeTransfer(Uuid),
    CancelTransfer(Uuid),
    CancelAllTransfers,
    
    // Progress events
    ProgressUpdate(TransferProgress),
    StatsUpdate(TransferStats),
    
    // Error events
    TransferError { task_id: Uuid, error: String },
    SystemError(String),
    
    // UI events
    ShowSettings,
    HideSettings,
    ToggleTheme,
    MinimizeToTray,
    ShowWindow,
    Quit,
}

pub trait UiEventHandler: Send + Sync {
    fn handle_event(&mut self, event: UiEvent);
}

pub struct EventBus {
    handlers: Vec<Box<dyn UiEventHandler>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn add_handler(&mut self, handler: Box<dyn UiEventHandler>) {
        self.handlers.push(handler);
    }

    pub fn emit(&mut self, event: UiEvent) {
        for handler in &mut self.handlers {
            handler.handle_event(event.clone());
        }
    }
}
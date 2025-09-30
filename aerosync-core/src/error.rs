use thiserror::Error;

pub type Result<T> = std::result::Result<T, AeroSyncError>;

#[derive(Error, Debug)]
pub enum AeroSyncError {
    #[error("File I/O error: {0}")]
    FileIo(#[from] std::io::Error),
    
    #[error("Network error: {0}")]
    Network(String),
    
    #[error("Storage error: {message}")]
    Storage { message: String },
    
    #[error("Transfer cancelled by user")]
    Cancelled,
    
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
    
    #[error("Protocol error: {0}")]
    Protocol(String),
    
    #[error("System error: {0}")]
    System(String),
    
    #[error("Unknown error: {0}")]
    Unknown(String),
}
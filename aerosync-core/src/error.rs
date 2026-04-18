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

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("TOML parsing error: {0}")]
    TomlParse(String),
}

// 为 TOML 错误实现 From trait
impl From<toml::de::Error> for AeroSyncError {
    fn from(err: toml::de::Error) -> Self {
        AeroSyncError::TomlParse(err.to_string())
    }
}

impl From<toml::ser::Error> for AeroSyncError {
    fn from(err: toml::ser::Error) -> Self {
        AeroSyncError::TomlParse(err.to_string())
    }
}

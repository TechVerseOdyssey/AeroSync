use crate::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub is_directory: bool,
    pub modified_time: SystemTime,
    pub permissions: FilePermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePermissions {
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

pub struct FileManager;

impl FileManager {
    pub fn new() -> Self {
        Self
    }

    pub async fn get_file_info<P: AsRef<Path>>(path: P) -> Result<FileInfo> {
        let path = path.as_ref();
        let metadata = tokio::fs::metadata(path).await?;
        
        let name = path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let permissions = FilePermissions {
            readable: true, // TODO: Implement proper permission checking per platform
            writable: !metadata.permissions().readonly(),
            executable: false, // TODO: Implement executable check per platform
        };

        Ok(FileInfo {
            path: path.to_path_buf(),
            name,
            size: metadata.len(),
            is_directory: metadata.is_dir(),
            modified_time: metadata.modified()?,
            permissions,
        })
    }

    pub async fn list_directory<P: AsRef<Path>>(path: P) -> Result<Vec<FileInfo>> {
        let path = path.as_ref();
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut files = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            match Self::get_file_info(entry.path()).await {
                Ok(file_info) => files.push(file_info),
                Err(e) => {
                    tracing::warn!("Failed to get info for file {:?}: {}", entry.path(), e);
                }
            }
        }

        files.sort_by(|a, b| {
            match (a.is_directory, b.is_directory) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            }
        });

        Ok(files)
    }

    pub async fn create_directory<P: AsRef<Path>>(path: P) -> Result<()> {
        tokio::fs::create_dir_all(path).await?;
        Ok(())
    }

    pub async fn file_exists<P: AsRef<Path>>(path: P) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    pub async fn get_available_space<P: AsRef<Path>>(_path: P) -> Result<u64> {
        // TODO: Implement platform-specific disk space checking
        // For now, return a large number
        Ok(u64::MAX)
    }

    pub fn validate_path<P: AsRef<Path>>(path: P) -> Result<()> {
        let path = path.as_ref();
        
        if path.to_string_lossy().is_empty() {
            return Err(AeroSyncError::InvalidConfig("Empty path".to_string()));
        }

        // Check for invalid characters (basic validation)
        let path_str = path.to_string_lossy();
        if path_str.contains('\0') {
            return Err(AeroSyncError::InvalidConfig("Path contains null character".to_string()));
        }

        Ok(())
    }
}
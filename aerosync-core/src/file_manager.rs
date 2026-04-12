use crate::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::AsyncReadExt;

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
        
        // Better handling for file names with non-ASCII characters (e.g., Chinese)
        let name = path.file_name()
            .and_then(|os_str| os_str.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                // Fallback to lossy conversion if not valid UTF-8
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

        let permissions = FilePermissions {
            readable: is_readable(path),
            writable: !metadata.permissions().readonly(),
            executable: is_executable(path),
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

    pub async fn get_available_space<P: AsRef<Path>>(path: P) -> Result<u64> {
        let path = path.as_ref();
        available_space(path)
    }

    pub fn validate_path<P: AsRef<Path>>(path: P) -> Result<()> {
        let path = path.as_ref();
        
        if path.to_string_lossy().is_empty() {
            return Err(AeroSyncError::InvalidConfig("Empty path".to_string()));
        }

        let path_str = path.to_string_lossy();
        if path_str.contains('\0') {
            return Err(AeroSyncError::InvalidConfig("Path contains null character".to_string()));
        }

        Ok(())
    }

    /// 计算文件的 SHA-256 哈希（十六进制字符串），流式分块读取，支持大文件
    pub async fn compute_sha256<P: AsRef<Path>>(path: P) -> Result<String> {
        const BUF_SIZE: usize = 1024 * 1024; // 1 MB chunks
        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BUF_SIZE];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    /// 校验文件 SHA-256 与期望值是否匹配
    pub async fn verify_sha256<P: AsRef<Path>>(path: P, expected: &str) -> Result<bool> {
        let actual = Self::compute_sha256(path).await?;
        Ok(actual == expected)
    }
}

// ── 平台相关辅助函数 ─────────────────────────────────────────────────────────

/// 检查路径对当前进程是否可读
fn is_readable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o444 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // Windows: 能获取到 metadata 即视为可读
        std::fs::metadata(path).is_ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::metadata(path).is_ok()
    }
}

/// 检查路径对当前进程是否可执行
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // Windows: 以扩展名判断是否可执行
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "exe" | "bat" | "cmd" | "com"))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// 返回指定路径所在文件系统的可用空间（字节）
fn available_space(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        // 路径需以 NUL 结尾传给 statvfs
        let cpath = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| AeroSyncError::InvalidConfig("Path contains null byte".to_string()))?;

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
        if ret != 0 {
            return Err(AeroSyncError::FileIo(std::io::Error::last_os_error()));
        }
        Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use winapi::um::fileapi::GetDiskFreeSpaceExW;
        use winapi::um::winnt::ULARGE_INTEGER;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut free_bytes: ULARGE_INTEGER = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(AeroSyncError::FileIo(std::io::Error::last_os_error()));
        }
        Ok(unsafe { *free_bytes.QuadPart() as u64 })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── helpers ───────────────────────────────────────────────────────────────

    async fn write_temp_file(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        tokio::fs::write(&path, content).await.unwrap();
        path
    }

    // ── SHA-256 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_compute_sha256_known_value() {
        let dir = TempDir::new().unwrap();
        // SHA-256 of empty string is known
        let path = write_temp_file(&dir, "empty.bin", b"").await;
        let hash = FileManager::compute_sha256(&path).await.unwrap();
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn test_compute_sha256_deterministic() {
        let dir = TempDir::new().unwrap();
        let content = b"hello aerosync";
        let path = write_temp_file(&dir, "data.bin", content).await;
        let hash1 = FileManager::compute_sha256(&path).await.unwrap();
        let hash2 = FileManager::compute_sha256(&path).await.unwrap();
        assert_eq!(hash1, hash2);
    }

    #[tokio::test]
    async fn test_compute_sha256_different_content() {
        let dir = TempDir::new().unwrap();
        let path1 = write_temp_file(&dir, "a.bin", b"content_a").await;
        let path2 = write_temp_file(&dir, "b.bin", b"content_b").await;
        let hash1 = FileManager::compute_sha256(&path1).await.unwrap();
        let hash2 = FileManager::compute_sha256(&path2).await.unwrap();
        assert_ne!(hash1, hash2);
    }

    #[tokio::test]
    async fn test_verify_sha256_match() {
        let dir = TempDir::new().unwrap();
        let path = write_temp_file(&dir, "c.bin", b"verify me").await;
        let hash = FileManager::compute_sha256(&path).await.unwrap();
        let ok = FileManager::verify_sha256(&path, &hash).await.unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn test_verify_sha256_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = write_temp_file(&dir, "d.bin", b"original").await;
        let ok = FileManager::verify_sha256(&path, "0000000000000000000000000000000000000000000000000000000000000000").await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn test_compute_sha256_nonexistent_file_errors() {
        let result = FileManager::compute_sha256("/nonexistent/path/file.bin").await;
        assert!(result.is_err());
    }

    // ── file_info ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_file_info_regular_file() {
        let dir = TempDir::new().unwrap();
        let content = b"hello world";
        let path = write_temp_file(&dir, "info.txt", content).await;
        let info = FileManager::get_file_info(&path).await.unwrap();
        assert_eq!(info.name, "info.txt");
        assert_eq!(info.size, content.len() as u64);
        assert!(!info.is_directory);
    }

    #[tokio::test]
    async fn test_get_file_info_directory() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        tokio::fs::create_dir(&sub).await.unwrap();
        let info = FileManager::get_file_info(&sub).await.unwrap();
        assert!(info.is_directory);
    }

    #[tokio::test]
    async fn test_get_file_info_nonexistent_errors() {
        let result = FileManager::get_file_info("/no/such/file.txt").await;
        assert!(result.is_err());
    }

    // ── list_directory ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_directory_returns_files() {
        let dir = TempDir::new().unwrap();
        write_temp_file(&dir, "z.bin", b"z").await;
        write_temp_file(&dir, "a.bin", b"a").await;

        let entries = FileManager::list_directory(dir.path()).await.unwrap();
        assert_eq!(entries.len(), 2);
        // Should be sorted alphabetically
        assert_eq!(entries[0].name, "a.bin");
        assert_eq!(entries[1].name, "z.bin");
    }

    #[tokio::test]
    async fn test_list_directory_dirs_before_files() {
        let dir = TempDir::new().unwrap();
        write_temp_file(&dir, "file.bin", b"data").await;
        tokio::fs::create_dir(dir.path().join("subdir")).await.unwrap();

        let entries = FileManager::list_directory(dir.path()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_directory, "first entry should be a directory");
        assert!(!entries[1].is_directory, "second entry should be a file");
    }

    #[tokio::test]
    async fn test_list_empty_directory() {
        let dir = TempDir::new().unwrap();
        let entries = FileManager::list_directory(dir.path()).await.unwrap();
        assert!(entries.is_empty());
    }

    // ── create_directory ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_directory_nested() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        FileManager::create_directory(&nested).await.unwrap();
        assert!(nested.exists());
    }

    #[tokio::test]
    async fn test_create_directory_idempotent() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("existing");
        FileManager::create_directory(&sub).await.unwrap();
        // Second call should not fail
        FileManager::create_directory(&sub).await.unwrap();
        assert!(sub.exists());
    }

    // ── file_exists ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_file_exists_true() {
        let dir = TempDir::new().unwrap();
        let path = write_temp_file(&dir, "exists.bin", b"hi").await;
        assert!(FileManager::file_exists(&path).await);
    }

    #[tokio::test]
    async fn test_file_exists_false() {
        assert!(!FileManager::file_exists("/no/such/path.bin").await);
    }

    // ── validate_path ─────────────────────────────────────────────────────────

    #[test]
    fn test_validate_path_valid() {
        assert!(FileManager::validate_path("/some/valid/path.txt").is_ok());
    }

    #[test]
    fn test_validate_path_with_null_byte_errors() {
        let bad_path = "/some/path\0/file";
        assert!(FileManager::validate_path(bad_path).is_err());
    }

    // ── large file sha256 ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_sha256_large_file() {
        let dir = TempDir::new().unwrap();
        // 1MB of zeros
        let content = vec![0u8; 1024 * 1024];
        let path = write_temp_file(&dir, "large.bin", &content).await;
        let hash = FileManager::compute_sha256(&path).await.unwrap();
        assert_eq!(hash.len(), 64); // hex-encoded 32 bytes
        // Verify consistency
        let hash2 = FileManager::compute_sha256(&path).await.unwrap();
        assert_eq!(hash, hash2);
    }
}
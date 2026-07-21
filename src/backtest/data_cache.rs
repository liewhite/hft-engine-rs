use std::path::{Path, PathBuf};

/// 历史数据缓存：纯 key -> bytes 存储，与"从哪下载"解耦。
///
/// key 直接镜像数据源路径 (如 `futures/um/daily/trades/BTCUSDT/BTCUSDT-trades-2024-01-01.zip`)。
pub trait DataCache: Send + Sync {
    fn get(&self, key: &str) -> Option<Vec<u8>>;
    fn put(&self, key: &str, data: &[u8]);
}

/// 本地文件系统缓存：key 作为 root 下的相对路径落盘。
pub struct LocalFsDataCache {
    root: PathBuf,
}

impl LocalFsDataCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl DataCache for LocalFsDataCache {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.root.join(key);
        if path.is_file() {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    tracing::debug!(key, "cache hit");
                    Some(bytes)
                }
                Err(e) => {
                    tracing::warn!(key, error = %e, "cache read failed");
                    None
                }
            }
        } else {
            None
        }
    }

    fn put(&self, key: &str, data: &[u8]) {
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(key, error = %e, "cache mkdir failed");
                return;
            }
        }
        match std::fs::write(&path, data) {
            Ok(()) => tracing::debug!(key, bytes = data.len(), "cache put"),
            Err(e) => tracing::warn!(key, error = %e, "cache write failed"),
        }
    }
}

impl AsRef<Path> for LocalFsDataCache {
    fn as_ref(&self) -> &Path {
        &self.root
    }
}

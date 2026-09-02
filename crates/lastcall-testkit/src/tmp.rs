//! Per-test temporary directories.
//!
//! Two flavours, on purpose:
//!
//! - [`TempDir::new`] lives under the platform temp dir (`$TMPDIR` on macOS). Fine for config
//!   files and fixture repositories.
//! - [`TempDir::socket_dir`] lives under `/tmp/lc-<pid>-<nanos>/`. **Every directory that will
//!   carry a Unix socket must use this one**: macOS caps `sun_path` at 104 bytes and
//!   `$TMPDIR/…/herdr.sock` overflows it (herdr uses `/tmp/hapi-<pid>-<nanos>` for exactly this
//!   reason, `tests/api_ping.rs:17-23`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// A directory removed (recursively) on drop.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A fresh directory under the platform temp dir, named `<prefix>-<pid>-<nanos>-<n>`.
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// A fresh directory at `/tmp/lc-<pid>-<nanos>-<n>/`, short enough for a Unix socket path.
    pub fn socket_dir() -> Self {
        let path = PathBuf::from(format!("/tmp/lc-{}", unique_suffix()));
        std::fs::create_dir_all(&path).expect("create socket temp dir");
        let dir = Self { path };
        let probe = dir.path().join("herdr.sock");
        assert!(
            probe.as_os_str().len() < 100,
            "socket dir path too long for sun_path: {}",
            probe.display()
        );
        dir
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `self.path().join(rel)`.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }

    /// Write a file under the directory, creating parents.
    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    /// Create a subdirectory.
    pub fn mkdir(&self, rel: impl AsRef<Path>) -> PathBuf {
        let path = self.path.join(rel);
        std::fs::create_dir_all(&path).expect("create subdir");
        path
    }

    /// Stop owning the directory (it is not removed on drop).
    pub fn into_path(mut self) -> PathBuf {
        std::mem::replace(&mut self.path, PathBuf::new())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_socket_dir_is_short_and_removed_on_drop() {
        let dir = TempDir::socket_dir();
        let path = dir.path().to_path_buf();
        assert!(path.to_string_lossy().starts_with("/tmp/lc-"));
        assert!(path.is_dir());
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn tmp_write_creates_parents() {
        let dir = TempDir::new("lc-tmp-test");
        let file = dir.write("a/b/c.txt", "hi");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "hi");
    }
}

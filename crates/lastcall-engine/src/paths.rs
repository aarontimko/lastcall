//! Storage identity and layout (docs/spec/00-spec.md §6.1; kickoff deliverable 2).
//!
//! `RootId` / `ParentId` are the first 16 hex chars of SHA-256 over the **canonicalized**
//! absolute path (`fs::canonicalize`: symlinks resolved, `/tmp` → `/private/tmp` on macOS).
//! The canonical path is recorded inside `meta.json` / `ledger.json`; a recorded path that
//! no longer canonicalizes to the same string is a different root (E4).
//!
//! ```text
//! <state>/roots/<parent-hash>/meta.json
//! <state>/roots/<parent-hash>/repos/<repo-hash>/{ledger.json, store/, index, index.tree, index.tmp, lock}
//! ```
//!
//! `index.tree`, `index.tmp` and `lock` are additive files under the frozen layout (reported
//! as a proposed §6.1 editorial addition).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Hash a canonical path into the 16-hex identity.
pub fn hash_path_id(canonical: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let digest = Sha256::digest(canonical.as_os_str().as_bytes());
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Identity of a parent directory (a `parent_dirs` entry, or a root's own parent).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ParentId(String);

impl ParentId {
    pub fn of(canonical: &Path) -> Self {
        Self(hash_path_id(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of a repo or draft root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct RootId(String);

impl RootId {
    pub fn of(canonical: &Path) -> Self {
        Self(hash_path_id(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RootId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every file a root owns under the state dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPaths {
    pub repo_dir: PathBuf,
    pub ledger: PathBuf,
    pub store: PathBuf,
    pub index: PathBuf,
    pub index_tree: PathBuf,
    pub index_tmp: PathBuf,
    pub lock: PathBuf,
}

impl RepoPaths {
    pub fn under(repo_dir: PathBuf) -> Self {
        Self {
            ledger: repo_dir.join("ledger.json"),
            store: repo_dir.join("store"),
            index: repo_dir.join("index"),
            index_tree: repo_dir.join("index.tree"),
            index_tmp: repo_dir.join("index.tmp"),
            lock: repo_dir.join("lock"),
            repo_dir,
        }
    }
}

/// The state-dir layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    state_dir: PathBuf,
}

impl Layout {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn roots_dir(&self) -> PathBuf {
        self.state_dir.join("roots")
    }

    pub fn parent_dir(&self, parent: &ParentId) -> PathBuf {
        self.roots_dir().join(parent.as_str())
    }

    pub fn meta_path(&self, parent: &ParentId) -> PathBuf {
        self.parent_dir(parent).join("meta.json")
    }

    pub fn repos_dir(&self, parent: &ParentId) -> PathBuf {
        self.parent_dir(parent).join("repos")
    }

    pub fn repo_paths(&self, parent: &ParentId, root: &RootId) -> RepoPaths {
        RepoPaths::under(self.repos_dir(parent).join(root.as_str()))
    }
}

/// `meta.json` under a parent dir.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParentMeta {
    pub schema_version: String,
    /// The canonical parent path.
    pub parent: String,
    pub created_at: String,
}

/// Canonicalize, with a clear error.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_id_is_sixteen_hex_of_sha256() {
        // sha256("/a") = 9d5ad5ab1b16b3a5f4c66ca9ff6ecb9d3f4c9e1c1b7f74b6f1a0f5cee5c0f9dc? — do not
        // pin the digest, pin the shape and determinism.
        let id = hash_path_id(Path::new("/a"));
        assert_eq!(id.len(), 16);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(id, hash_path_id(Path::new("/a")));
        assert_ne!(id, hash_path_id(Path::new("/b")));
        assert_ne!(
            id,
            hash_path_id(Path::new("/a/")),
            "byte-exact: no normalization here"
        );
        assert_eq!(
            RootId::of(Path::new("/x")).as_str(),
            ParentId::of(Path::new("/x")).as_str()
        );
    }

    #[test]
    fn paths_layout_matches_the_spec_tree() {
        let layout = Layout::new("/state");
        let parent = ParentId::of(Path::new("/srv/code"));
        let root = RootId::of(Path::new("/srv/code/repo"));
        assert_eq!(
            layout.meta_path(&parent),
            PathBuf::from(format!("/state/roots/{}/meta.json", parent.as_str()))
        );
        let repo = layout.repo_paths(&parent, &root);
        let base = format!("/state/roots/{}/repos/{}", parent.as_str(), root.as_str());
        assert_eq!(repo.repo_dir, PathBuf::from(&base));
        assert_eq!(repo.ledger, PathBuf::from(format!("{base}/ledger.json")));
        assert_eq!(repo.store, PathBuf::from(format!("{base}/store")));
        assert_eq!(repo.index, PathBuf::from(format!("{base}/index")));
        assert_eq!(repo.index_tree, PathBuf::from(format!("{base}/index.tree")));
        assert_eq!(repo.index_tmp, PathBuf::from(format!("{base}/index.tmp")));
        assert_eq!(repo.lock, PathBuf::from(format!("{base}/lock")));
    }

    #[test]
    fn paths_canonicalize_resolves_symlinks() {
        let dir = lastcall_testkit::tmp::TempDir::new("lc-paths");
        let real = dir.mkdir("real");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(canonicalize(&link).unwrap(), canonicalize(&real).unwrap());
        assert_ne!(RootId::of(&link), RootId::of(&canonicalize(&link).unwrap()));
        assert!(canonicalize(&dir.join("missing")).is_err());
    }
}

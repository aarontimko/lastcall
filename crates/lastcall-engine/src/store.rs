//! The private object store (docs/spec/00-spec.md §6.1; kickoff deliverable 2).
//!
//! A bare git repository we own. For git roots, `objects/info/alternates` points at the
//! user's object directory (the common dir for linked worktrees, D10), so unchanged content
//! is referenced, not duplicated; the normalization keys (`core.autocrlf`, `core.eol`,
//! `core.filemode`, `core.ignorecase`) and the user's `info/attributes` are copied in **at
//! every open**. Draft roots get the same store with no alternates and no key copy.
//!
//! **Never run `gc`, `prune`, `repack -d`, or `fsck --lost-found` in the store**: nothing in
//! it is anchored by a ref, so pruning would delete the seen tree and every override blob.
//! Plumbing commands never trigger auto-gc; we never run `commit`/`merge` here. The store
//! grows by one loose object per scanned version of each changed file (accepted for v1;
//! hardening shape: `refs/lastcall/seen` + an overrides tree ref, after which gc is safe).
//!
//! Git skips the physical write when an object already exists through the alternate, so
//! "in the store" means "resolvable through the store": the seen tree, every subtree and
//! every unchanged blob live only in the user's objects dir. [`Store::exists`] before
//! trusting any oid (§6.1 verify-on-read).
//!
//! Clean filters run inside `hash-object -w` with `GIT_DIR=<store>`; a `filter=lfs`
//! attribute executes `git-lfs clean` (writes under `<store>/lfs/` or fails when git-lfs is
//! missing, which surfaces as [`Current::Unhashable`]). Informational deferral.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::env::Env;
use crate::git::{self, BatchCheck, ConfigList, GitError, Mode, Oid, RepoGit, StoreGit};
use crate::paths::RepoPaths;

/// `git | draft`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootKind {
    Git,
    Draft,
}

/// Config keys copied from the user's repo into the store (§6.1).
pub const COPIED_CONFIG_KEYS: &[&str] = &[
    "core.autocrlf",
    "core.eol",
    "core.filemode",
    "core.ignorecase",
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("store io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("store: {0}")]
    Other(String),
}

fn io_err(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// What the work tree currently holds at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Current {
    /// `lstat` failed with NotFound.
    Absent,
    /// Present but cannot be hashed: a directory where a file was, EACCES, a failing clean
    /// filter. Always a pending row, never a skip.
    Unhashable(String),
    Present {
        oid: Oid,
        mode: Mode,
    },
}

/// One entry to write into a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeWrite {
    Set { path: Vec<u8>, mode: Mode, oid: Oid },
    Remove { path: Vec<u8> },
}

/// The private store for one root.
#[derive(Debug)]
pub struct Store {
    git: Arc<StoreGit>,
    dir: PathBuf,
    kind: RootKind,
    index_tmp: PathBuf,
    filemode: bool,
}

/// Everything about the user's repository the store needs, read **once** by the caller.
///
/// Phase 5 deliverable 1c: before this, `Store::open` spawned seven `git` children of its
/// own per root (`config --get core.excludesfile`, four `COPIED_CONFIG_KEYS` reads, and
/// two `rev-parse --git-path`). All of it now comes from one `config --list -z` and one
/// batched `rev-parse` the caller already ran for its head inspection.
#[derive(Debug, Clone)]
pub struct RepoFacts<'a> {
    /// One `config --list -z` of the user's repository.
    pub config: &'a ConfigList,
    /// `rev-parse --git-path objects` — the object dir the alternate points at (the
    /// common dir's for a linked worktree, D10).
    pub objects_dir: PathBuf,
    /// `rev-parse --git-path info/attributes`.
    pub info_attributes: PathBuf,
}

impl<'a> RepoFacts<'a> {
    /// Read the two git paths with a `--git-path` call each. The engine's `open_root`
    /// folds them into the head inspection's batched `rev-parse` instead and builds the
    /// struct literally; this is for callers with no inspection to fold them into.
    pub fn read(git: &RepoGit, config: &'a ConfigList) -> Result<Self, GitError> {
        Ok(Self {
            config,
            objects_dir: git.git_path("objects")?,
            info_attributes: git.git_path("info/attributes")?,
        })
    }
}

impl Store {
    /// Open (initializing when absent) the store for `root`. `repo` is `Some` for git
    /// roots. Returns the store and any notices (a failed key copy is a notice, never fatal).
    pub fn open(
        env: &Env,
        root: &Path,
        kind: RootKind,
        paths: &RepoPaths,
        repo: Option<&RepoFacts<'_>>,
    ) -> Result<(Self, Vec<String>), StoreError> {
        let mut notices = Vec::new();
        std::fs::create_dir_all(&paths.repo_dir).map_err(|e| io_err(&paths.repo_dir, e))?;
        if !paths.store.join("HEAD").is_file() {
            StoreGit::init_bare(env, &paths.store)?;
        }
        // A stale temp index from a crashed fold; read-tree would replace it anyway.
        let _ = std::fs::remove_file(&paths.index_tmp);

        let excludes_file = repo
            .and_then(|r| r.config.get("core.excludesfile"))
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let git =
            StoreGit::new(env, root, &paths.store, &paths.index).with_excludes_file(excludes_file);
        // What the store's own config file already holds, in one spawn: every write below
        // is skipped when the value already matches.
        let store_config = git.config_list_local();
        // The store's own config pins the monitor/cache keys as well (a `git` run by hand
        // against the store must not start a daemon either). A failure is a notice: the
        // `-c` on every command still holds.
        for (key, value) in git::NEUTRALIZED_CONFIG {
            if store_config.get(key) == Some(*value) {
                continue;
            }
            if let Err(e) = git.run(&["config", key, value]) {
                notices.push(format!("cannot set {key} in the store: {e}"));
            }
        }

        if let Some(r) = repo {
            // Alternates → the user's object dir (the common dir for a linked worktree).
            let info = paths.store.join("objects").join("info");
            std::fs::create_dir_all(&info).map_err(|e| io_err(&info, e))?;
            let alternates = info.join("alternates");
            let mut line = r.objects_dir.as_os_str().as_bytes().to_vec();
            line.push(b'\n');
            std::fs::write(&alternates, line).map_err(|e| io_err(&alternates, e))?;
            // Normalization keys, at every open (the user may change them).
            for key in COPIED_CONFIG_KEYS {
                match r.config.get(key) {
                    Some(v) => {
                        if store_config.get(key) == Some(v) {
                            continue;
                        }
                        if let Err(e) = git.run(&["config", key, v]) {
                            notices.push(format!("cannot copy {key} into the store: {e}"));
                        }
                    }
                    None => {
                        // Unset in the user's config: unset ours (exit 5 = was not set).
                        if store_config.get(key).is_some() {
                            let _ = git.run_raw(None, &["config", "--unset", key], None);
                        }
                    }
                }
            }
            // info/attributes: invisible to the store otherwise; a `* text=auto` living only
            // there would make every CRLF file churn.
            let theirs = &r.info_attributes;
            let ours = paths.store.join("info").join("attributes");
            match std::fs::read(theirs) {
                Ok(bytes) => {
                    if let Some(parent) = ours.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
                    }
                    std::fs::write(&ours, bytes).map_err(|e| io_err(&ours, e))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = std::fs::remove_file(&ours);
                }
                Err(e) => notices.push(format!("cannot read {}: {e}", theirs.display())),
            }
        }

        let filemode = match git.run_raw(None, &["config", "--get", "core.filemode"], None) {
            Ok(out) if out.success() => out.stdout_trimmed() != "false",
            _ => true,
        };
        Ok((
            Self {
                git: Arc::new(git),
                dir: paths.store.clone(),
                kind,
                index_tmp: paths.index_tmp.clone(),
                filemode,
            },
            notices,
        ))
    }

    pub fn git(&self) -> &Arc<StoreGit> {
        &self.git
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn kind(&self) -> RootKind {
        self.kind
    }

    pub fn root(&self) -> &Path {
        self.git.root()
    }

    /// The store's effective `core.filemode` (copied from the user's repo).
    pub fn filemode(&self) -> bool {
        self.filemode
    }

    /// The mode git would record for `metadata` (an `lstat` result).
    pub fn mode_of(&self, meta: &std::fs::Metadata) -> Mode {
        use std::os::unix::fs::PermissionsExt;
        if meta.file_type().is_symlink() {
            Mode::Symlink
        } else if self.filemode && meta.permissions().mode() & 0o111 != 0 {
            Mode::Executable
        } else {
            Mode::Regular
        }
    }

    /// `lstat` → [`Current`], hashing through `hash-object -w` with cwd = root and the
    /// root-relative path (the `text=auto` rule); symlinks via `readlink` + `--stdin`.
    pub fn hash_path(&self, rel: &[u8]) -> Current {
        let full = self.git.root().join(OsStr::from_bytes(rel));
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Current::Absent,
            Err(e) => return Current::Unhashable(e.to_string()),
        };
        self.hash_with_meta(rel, &full, &meta)
    }

    fn hash_with_meta(&self, rel: &[u8], full: &Path, meta: &std::fs::Metadata) -> Current {
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = match std::fs::read_link(full) {
                Ok(t) => t,
                Err(e) => return Current::Unhashable(format!("readlink: {e}")),
            };
            return match self.hash_bytes(target.as_os_str().as_bytes()) {
                Ok(oid) => Current::Present {
                    oid,
                    mode: Mode::Symlink,
                },
                Err(e) => Current::Unhashable(e.to_string()),
            };
        }
        if ft.is_dir() {
            return Current::Unhashable("typechange: a directory where a file was".into());
        }
        if !ft.is_file() {
            return Current::Unhashable("not a regular file".into());
        }
        let mode = self.mode_of(meta);
        match self.git.run(&[
            OsStr::new("hash-object"),
            OsStr::new("-w"),
            OsStr::new("--"),
            OsStr::from_bytes(rel),
        ]) {
            Ok(out) => match Oid::parse(String::from_utf8_lossy(&out).trim()) {
                Some(oid) => Current::Present { oid, mode },
                None => Current::Unhashable("hash-object printed no oid".into()),
            },
            Err(e) => Current::Unhashable(e.to_string()),
        }
    }

    /// Whether `--stdin-paths` reads a name back verbatim: it C-unquotes a line that starts
    /// with `"` and treats control bytes (`\n`, `\r`, …) as line structure.
    fn batchable(rel: &[u8]) -> bool {
        rel.first() != Some(&b'"') && !rel.iter().any(|b| *b < 0x20)
    }

    /// Hash many paths: regular files go through one `hash-object -w --stdin-paths` call;
    /// symlinks, directories and names `--stdin-paths` would misread take the single-path
    /// route.
    pub fn hash_paths(&self, rels: &[Vec<u8>]) -> Vec<Current> {
        let mut out: Vec<Option<Current>> = vec![None; rels.len()];
        let mut batch: Vec<usize> = Vec::new();
        let mut metas: Vec<Option<std::fs::Metadata>> = Vec::with_capacity(rels.len());
        for (i, rel) in rels.iter().enumerate() {
            let full = self.git.root().join(OsStr::from_bytes(rel));
            match std::fs::symlink_metadata(&full) {
                Ok(m) => {
                    if m.file_type().is_file() && Self::batchable(rel) {
                        batch.push(i);
                    }
                    metas.push(Some(m));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    out[i] = Some(Current::Absent);
                    metas.push(None);
                }
                Err(e) => {
                    out[i] = Some(Current::Unhashable(e.to_string()));
                    metas.push(None);
                }
            }
        }
        if !batch.is_empty() {
            let mut stdin = Vec::new();
            for &i in &batch {
                stdin.extend_from_slice(&rels[i]);
                stdin.push(b'\n');
            }
            if let Ok(bytes) =
                self.git
                    .run_stdin(None, &["hash-object", "-w", "--stdin-paths"], &stdin)
            {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect();
                if lines.len() == batch.len() {
                    for (k, &i) in batch.iter().enumerate() {
                        if let (Some(oid), Some(meta)) = (Oid::parse(lines[k]), metas[i].as_ref()) {
                            out[i] = Some(Current::Present {
                                oid,
                                mode: self.mode_of(meta),
                            });
                        }
                    }
                }
            }
            // Anything the batch could not answer (EACCES, a race, a filter failure) takes
            // the single-path route, which reports the reason per path.
        }
        for (i, rel) in rels.iter().enumerate() {
            if out[i].is_none() {
                let full = self.git.root().join(OsStr::from_bytes(rel));
                out[i] = Some(match metas[i].as_ref() {
                    Some(meta) => self.hash_with_meta(rel, &full, meta),
                    None => self.hash_path(rel),
                });
            }
        }
        out.into_iter()
            .map(|c| c.unwrap_or(Current::Absent))
            .collect()
    }

    /// `hash-object -w --stdin` (no `--path`: the bytes are already-normalized baseline
    /// content, or a symlink's link text).
    pub fn hash_bytes(&self, bytes: &[u8]) -> Result<Oid, StoreError> {
        let out = self
            .git
            .run_stdin(None, &["hash-object", "-w", "--stdin"], bytes)?;
        Oid::parse(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| StoreError::Other("hash-object --stdin printed no oid".into()))
    }

    /// `cat-file blob <oid>`.
    pub fn cat_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError> {
        Ok(self.git.run(&["cat-file", "blob", oid.as_str()])?)
    }

    /// Every blob in one `cat-file --batch` (one process for a whole scan, not one per
    /// row). Missing, ambiguous and non-blob objects are simply absent from the map.
    pub fn cat_blobs(&self, oids: &[Oid]) -> Result<HashMap<Oid, Vec<u8>>, StoreError> {
        let mut out = HashMap::new();
        if oids.is_empty() {
            return Ok(out);
        }
        let mut stdin = Vec::new();
        for oid in oids {
            stdin.extend_from_slice(oid.as_str().as_bytes());
            stdin.push(b'\n');
        }
        let bytes = self.git.run_stdin(None, &["cat-file", "--batch"], &stdin)?;
        // `<oid> <type> <size>\n<content>\n` per found object; `<obj> missing\n` otherwise.
        let mut rest: &[u8] = &bytes;
        while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
            let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
            rest = &rest[nl + 1..];
            let fields: Vec<&str> = header.split(' ').collect();
            let (Some(oid), Some(kind), Some(size)) = (
                fields.first().and_then(|o| Oid::parse(o)),
                fields.get(1),
                fields.get(2).and_then(|s| s.parse::<usize>().ok()),
            ) else {
                continue; // `missing` / `ambiguous`: nothing follows the header
            };
            if rest.len() < size {
                return Err(StoreError::Other(format!(
                    "cat-file --batch: truncated content for {oid}"
                )));
            }
            if *kind == "blob" {
                out.insert(oid, rest[..size].to_vec());
            }
            rest = &rest[size..];
            if rest.first() == Some(&b'\n') {
                rest = &rest[1..];
            }
        }
        Ok(out)
    }

    /// Whether `oid` resolves through the store (own objects or the alternate).
    pub fn exists(&self, oid: &Oid) -> bool {
        self.exists_many(std::slice::from_ref(oid))[0]
    }

    /// Batched `cat-file --batch-check`; an unanswerable query counts as missing.
    pub fn exists_many(&self, oids: &[Oid]) -> Vec<bool> {
        if oids.is_empty() {
            return Vec::new();
        }
        let mut stdin = Vec::new();
        for oid in oids {
            stdin.extend_from_slice(oid.as_str().as_bytes());
            stdin.push(b'\n');
        }
        match self
            .git
            .run_stdin(None, &["cat-file", "--batch-check"], &stdin)
        {
            Ok(bytes) => {
                let answers = git::parse_batch_check(&bytes);
                oids.iter()
                    .enumerate()
                    .map(|(i, _)| matches!(answers.get(i), Some(BatchCheck::Present { .. })))
                    .collect()
            }
            Err(_) => vec![false; oids.len()],
        }
    }

    /// `ls-tree -r -z <tree>` → path → (mode, oid). A missing tree is an error (callers
    /// decide the fail-open policy).
    pub fn ls_tree(&self, tree: &Oid) -> Result<BTreeMap<Vec<u8>, (Mode, Oid)>, StoreError> {
        let out = self.git.run(&["ls-tree", "-r", "-z", tree.as_str()])?;
        let entries = git::parse_ls_tree_z(&out).map_err(StoreError::Other)?;
        Ok(entries
            .into_iter()
            .map(|e| (e.path, (e.mode, e.oid)))
            .collect())
    }

    /// Seed the temp index from `base` (or empty), apply `entries` via
    /// `update-index -z --index-info`, `write-tree`. The temp index is unlinked afterwards.
    pub fn write_tree(&self, base: Option<&Oid>, entries: &[TreeWrite]) -> Result<Oid, StoreError> {
        let _ = std::fs::remove_file(&self.index_tmp);
        self.seed_tmp_index(base)?;
        if !entries.is_empty() {
            let mut stdin = Vec::new();
            for e in entries {
                match e {
                    TreeWrite::Set { path, mode, oid } => {
                        stdin.extend_from_slice(mode.as_str().as_bytes());
                        stdin.push(b' ');
                        stdin.extend_from_slice(oid.as_str().as_bytes());
                        stdin.push(b'\t');
                        stdin.extend_from_slice(path);
                        stdin.push(0);
                    }
                    TreeWrite::Remove { path } => {
                        stdin.extend_from_slice(b"0 ");
                        stdin.extend_from_slice(Oid::zero().as_str().as_bytes());
                        stdin.push(b'\t');
                        stdin.extend_from_slice(path);
                        stdin.push(0);
                    }
                }
            }
            self.git.run_stdin(
                Some(&self.index_tmp),
                &["update-index", "-z", "--index-info"],
                &stdin,
            )?;
        }
        let out = self.git.run_with_index(&self.index_tmp, &["write-tree"]);
        let _ = std::fs::remove_file(&self.index_tmp);
        let out = out?;
        Oid::parse(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| StoreError::Other("write-tree printed no oid".into()))
    }

    fn seed_tmp_index(&self, base: Option<&Oid>) -> Result<(), StoreError> {
        match base {
            Some(tree) => self
                .git
                .run_with_index(&self.index_tmp, &["read-tree", tree.as_str()])?,
            None => self
                .git
                .run_with_index(&self.index_tmp, &["read-tree", "--empty"])?,
        };
        Ok(())
    }

    /// A tree of the current disk content (draft first sight): a temp index,
    /// `update-index --add -z --stdin` over the file list (which writes the blobs), then
    /// `write-tree`. Never `git add`. Skips `.git` entries and nested repositories.
    pub fn tree_of_disk(&self) -> Result<Oid, StoreError> {
        let mut files: Vec<Vec<u8>> = Vec::new();
        walk_files(self.git.root(), Path::new(""), &mut files)?;
        files.sort();
        let _ = std::fs::remove_file(&self.index_tmp);
        self.seed_tmp_index(None)?;
        if !files.is_empty() {
            let mut stdin = Vec::new();
            for f in &files {
                stdin.extend_from_slice(f);
                stdin.push(0);
            }
            self.git.run_stdin(
                Some(&self.index_tmp),
                &["update-index", "--add", "-z", "--stdin"],
                &stdin,
            )?;
        }
        let out = self.git.run_with_index(&self.index_tmp, &["write-tree"]);
        let _ = std::fs::remove_file(&self.index_tmp);
        let out = out?;
        Oid::parse(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| StoreError::Other("write-tree printed no oid".into()))
    }
}

fn walk_files(root: &Path, rel: &Path, out: &mut Vec<Vec<u8>>) -> Result<(), StoreError> {
    let dir = root.join(rel);
    let entries = std::fs::read_dir(&dir).map_err(|e| io_err(&dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let child_rel = rel.join(&name);
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            // A nested repository is another root (D9).
            if root.join(&child_rel).join(".git").exists() {
                continue;
            }
            walk_files(root, &child_rel, out)?;
        } else if ft.is_file() || ft.is_symlink() {
            out.push(child_rel.as_os_str().as_bytes().to_vec());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lastcall_testkit::fixture_repo::FixtureRepo;
    use lastcall_testkit::tmp::TempDir;

    /// In-crate twin of the testkit's `FixtureRepo::engine_env` (the testkit's `Env` is a
    /// different crate instance from a lib test's point of view).
    pub(crate) fn fixture_env(repo: &FixtureRepo, state: &TempDir) -> Env {
        let home = repo.parent_dir().join("home");
        std::fs::create_dir_all(&home).unwrap();
        Env::empty(repo.path())
            .with_home(home)
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
            .with_var("GIT_CONFIG_NOSYSTEM", "1")
            .with_var("LASTCALL_STATE_DIR", state.path().to_string_lossy())
    }

    fn open_git(repo: &FixtureRepo, state: &TempDir) -> (Store, RepoGit) {
        let env = fixture_env(repo, state);
        let paths = RepoPaths::under(state.join("repo"));
        let rg = RepoGit::new(&env, repo.path());
        let config = rg.config_list().unwrap();
        let facts = RepoFacts::read(&rg, &config).unwrap();
        let (store, notices) =
            Store::open(&env, repo.path(), RootKind::Git, &paths, Some(&facts)).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        (store, rg)
    }

    /// The one batched read must answer **exactly** what the seven `config --get` calls it
    /// replaced answered, for every key the open consumes — against real git, with values
    /// that contain the two bytes a naive `key=value` split would break on.
    #[test]
    fn store_config_list_agrees_with_config_get_for_every_key_the_open_reads() {
        let repo = FixtureRepo::new("cfg").unwrap();
        let state = TempDir::new("lc-cfg");
        let env = fixture_env(&repo, &state);
        let rg = RepoGit::new(&env, repo.path());
        // A URL with `=` in its query, an excludesfile whose name contains `=`, a
        // multi-valued key (last wins), and a key left unset.
        repo.git(&["config", "remote.origin.url", "https://h.invalid/r?a=1&b=2"])
            .unwrap();
        repo.git(&["config", "core.excludesfile", "/tmp/ex=cludes"])
            .unwrap();
        repo.git(&["config", "user.email", "a@b.invalid"]).unwrap();
        repo.git(&["config", "core.autocrlf", "input"]).unwrap();
        repo.git(&["config", "--add", "core.ignorecase", "false"])
            .unwrap();
        repo.git(&["config", "--add", "core.ignorecase", "true"])
            .unwrap();

        let mut keys: Vec<&str> = vec![
            "core.excludesfile",
            "user.email",
            "remote.origin.url",
            "core.eol",
        ];
        keys.extend_from_slice(COPIED_CONFIG_KEYS);
        let list = rg.config_list().unwrap();
        for key in keys {
            let want = rg.config_get(key).unwrap();
            // `--get` on an unset key exits 1 (`None`); a key set to the empty string is
            // `Some("")`. The map must make the same distinction.
            assert_eq!(
                list.get(key).map(str::to_owned),
                want,
                "config --list -z disagrees with config --get {key}"
            );
        }
        assert_eq!(
            list.get("core.ignorecase"),
            Some("true"),
            "the last value of a multi-valued key, like --get"
        );
        assert_eq!(list.get("no.such.key"), None);
    }

    #[test]
    fn store_open_writes_alternates_copies_keys_and_attributes() {
        let repo = FixtureRepo::new("store").unwrap();
        repo.git(&["config", "core.autocrlf", "input"]).unwrap();
        std::fs::write(repo.path().join(".git/info/attributes"), "* text=auto\n").unwrap();
        let state = TempDir::new("lc-store");
        let (store, rg) = open_git(&repo, &state);
        let alternates =
            std::fs::read_to_string(store.dir().join("objects/info/alternates")).unwrap();
        assert_eq!(
            alternates.trim(),
            rg.git_path("objects").unwrap().to_string_lossy()
        );
        let v = store
            .git()
            .run(&["config", "--get", "core.autocrlf"])
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&v).trim(), "input");
        assert_eq!(
            std::fs::read_to_string(store.dir().join("info/attributes")).unwrap(),
            "* text=auto\n"
        );
        // Second open: the user unset the key and removed the attributes file.
        repo.git(&["config", "--unset", "core.autocrlf"]).unwrap();
        std::fs::remove_file(repo.path().join(".git/info/attributes")).unwrap();
        let (store, _) = open_git(&repo, &state);
        let out = store
            .git()
            .run_raw(None, &["config", "--get", "core.autocrlf"], None)
            .unwrap();
        assert!(!out.success(), "key must be unset again");
        assert!(!store.dir().join("info/attributes").exists());
        assert!(store.filemode());
    }

    #[test]
    fn store_hash_path_symlink_dir_absent_and_batch() {
        let repo = FixtureRepo::new("hash").unwrap();
        let state = TempDir::new("lc-store");
        let (store, _) = open_git(&repo, &state);
        // Tracked, unchanged: resolvable through the alternate, oid equals HEAD's blob.
        let head_blob = repo.git(&["rev-parse", "HEAD:f1"]).unwrap();
        let cur = store.hash_path(b"f1");
        match &cur {
            Current::Present { oid, mode } => {
                assert_eq!(oid.as_str(), head_blob.trim());
                assert_eq!(*mode, Mode::Regular);
                assert!(store.exists(oid));
            }
            other => panic!("{other:?}"),
        }
        std::os::unix::fs::symlink("f1", repo.path().join("link")).unwrap();
        let link = store.hash_path(b"link");
        let link_oid = match &link {
            Current::Present { oid, mode } => {
                assert_eq!(*mode, Mode::Symlink);
                oid.clone()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            store.cat_blob(&link_oid).unwrap(),
            b"f1",
            "link text, not target"
        );
        assert_ne!(
            link_oid.as_str(),
            head_blob.trim(),
            "never follows the link"
        );
        std::fs::create_dir(repo.path().join("f2.d")).unwrap();
        assert!(matches!(store.hash_path(b"f2.d"), Current::Unhashable(_)));
        assert_eq!(store.hash_path(b"nope"), Current::Absent);

        let calls_before = store.git().hash_object_calls();
        let batch = store.hash_paths(&[
            b"f1".to_vec(),
            b"link".to_vec(),
            b"nope".to_vec(),
            b"f2.d".to_vec(),
            b"f2".to_vec(),
        ]);
        assert_eq!(batch[0], cur);
        assert_eq!(batch[1], link);
        assert_eq!(batch[2], Current::Absent);
        assert!(matches!(batch[3], Current::Unhashable(_)));
        assert!(matches!(batch[4], Current::Present { .. }));
        assert_eq!(
            store.git().hash_object_calls() - calls_before,
            2,
            "one --stdin-paths batch for f1+f2, one --stdin for the link"
        );
    }

    #[test]
    fn store_hash_paths_routes_quoted_and_control_names_around_the_batch() {
        let repo = FixtureRepo::new("hash-quote").unwrap();
        let state = TempDir::new("lc-store");
        let (store, _) = open_git(&repo, &state);
        let odd: [(&[u8], &[u8]); 3] = [
            (b"\"quoted", b"starts with a double quote\n"),
            (b"cr\rname", b"carriage return\n"),
            (b"nl\nname", b"newline\n"),
        ];
        for (name, content) in odd {
            std::fs::write(repo.path().join(OsStr::from_bytes(name)), content).unwrap();
        }
        let calls_before = store.git().hash_object_calls();
        let rels: Vec<Vec<u8>> = std::iter::once(b"f1".to_vec())
            .chain(odd.iter().map(|(n, _)| n.to_vec()))
            .collect();
        let batch = store.hash_paths(&rels);
        assert_eq!(
            store.git().hash_object_calls() - calls_before,
            4,
            "one batch for f1, one single call per odd name"
        );
        for (k, (name, content)) in odd.iter().enumerate() {
            let got = &batch[k + 1];
            assert_eq!(*got, store.hash_path(name), "{name:?}");
            match got {
                Current::Present { oid, .. } => {
                    assert_eq!(store.cat_blob(oid).unwrap(), *content, "{name:?}")
                }
                other => panic!("{name:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn store_write_tree_and_ls_tree_round_trip_with_removals_and_newlines() {
        let repo = FixtureRepo::new("tree").unwrap();
        let state = TempDir::new("lc-store");
        let (store, _) = open_git(&repo, &state);
        let head_tree =
            Oid::parse(repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap().trim()).unwrap();
        let listed = store.ls_tree(&head_tree).unwrap();
        assert_eq!(listed.len(), 3);
        let novel = store.hash_bytes(b"novel content\n").unwrap();
        let tree = store
            .write_tree(
                Some(&head_tree),
                &[
                    TreeWrite::Set {
                        path: b"new\nline".to_vec(),
                        mode: Mode::Executable,
                        oid: novel.clone(),
                    },
                    TreeWrite::Remove {
                        path: b"f3".to_vec(),
                    },
                ],
            )
            .unwrap();
        let after = store.ls_tree(&tree).unwrap();
        assert_eq!(after.len(), 3);
        assert!(!after.contains_key(&b"f3"[..]));
        assert_eq!(after[&b"new\nline"[..]], (Mode::Executable, novel));
        assert!(
            !state.join("repo/index.tmp").exists(),
            "temp index unlinked"
        );
        let empty = store.write_tree(None, &[]).unwrap();
        assert!(store.ls_tree(&empty).unwrap().is_empty());
        assert!(!store.exists(&Oid::parse(&"d".repeat(40)).unwrap()));
    }

    #[test]
    fn store_tree_of_disk_for_a_draft_root() {
        let dir = TempDir::new("lc-draft");
        let root = dir.mkdir("notes");
        dir.write("notes/a.md", "a\n");
        dir.write("notes/sub/b.md", "b\n");
        dir.write("notes/nested/.git/HEAD", "ref: refs/heads/main\n");
        dir.write("notes/nested/x", "x\n");
        std::os::unix::fs::symlink("a.md", root.join("l")).unwrap();
        let state = dir.mkdir("state");
        let env = Env::empty(dir.path()).with_home(dir.mkdir("home"));
        let paths = RepoPaths::under(state.join("repo"));
        let (store, notices) = Store::open(&env, &root, RootKind::Draft, &paths, None).unwrap();
        assert!(notices.is_empty());
        assert!(!store.dir().join("objects/info/alternates").exists());
        let tree = store.tree_of_disk().unwrap();
        let entries = store.ls_tree(&tree).unwrap();
        let paths: Vec<&[u8]> = entries.keys().map(Vec::as_slice).collect();
        assert_eq!(paths, vec![&b"a.md"[..], &b"l"[..], &b"sub/b.md"[..]]);
        assert_eq!(entries[&b"l"[..]].0, Mode::Symlink);
        assert!(store.exists(&entries[&b"a.md"[..]].1), "blobs were written");
    }
}

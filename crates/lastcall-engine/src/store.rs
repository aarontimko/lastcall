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
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::env::Env;
use crate::git::{self, BatchCheck, ConfigList, GitError, Mode, Oid, RepoGit, StoreGit};
use crate::paths::{self, RepoPaths};

/// `git | draft`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootKind {
    Git,
    Draft,
}

/// A folder another root looks after, as a root-relative path, with whether everything
/// below it belongs there (`true`, a folder watched with its whole tree) or only the files
/// directly inside it (`false`, a folder watched on its own).
pub type ExcludedDir = (Vec<u8>, bool);

/// What one watched folder's record covers (Amendment v1.13).
///
/// Naming a folder covers the files directly inside it. Adding `/**` covers the tree below
/// it as well, minus anything a repository or a more specific entry looks after. Either
/// way a file at or above `max_bytes` is listed but never read, so pointing lastcall at a
/// folder that happens to hold a disk image or a video never turns into copying it.
///
/// One value answers the question for every surface that asks it — the first sight's disk
/// walk, a scan's candidate list and the trim that prunes a record after the setting
/// changes — so the three can never disagree about what belongs to a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftScope {
    /// Whether the tree below the folder is covered too.
    pub recursive: bool,
    /// `collapse_size_bytes`: a regular file this big or bigger is never read.
    pub max_bytes: u64,
}

impl DraftScope {
    /// The folder on its own.
    pub fn plain(max_bytes: u64) -> Self {
        Self {
            recursive: false,
            max_bytes,
        }
    }

    /// The folder and the tree below it.
    pub fn tree(max_bytes: u64) -> Self {
        Self {
            recursive: true,
            max_bytes,
        }
    }

    /// Whether `rel`'s **shape** is covered: a folder watched on its own covers only what
    /// sits directly inside it. Size is a separate question on purpose — what a folder
    /// lists and what it reads are not the same thing, so a file that grew past the limit
    /// keeps its row instead of vanishing.
    pub fn admits_shape(&self, rel: &[u8]) -> bool {
        self.recursive || !rel.contains(&b'/')
    }

    /// Whether `rel` is covered **and** small enough to read, given its `lstat`.
    pub fn admits(&self, rel: &[u8], meta: &std::fs::Metadata) -> bool {
        self.admits_shape(rel) && !self.too_big(meta)
    }

    /// Whether this file is at or above the size at which reading stops. A symlink's own
    /// size is the text of the link, which is always short enough; a folder is not a file.
    pub fn too_big(&self, meta: &std::fs::Metadata) -> bool {
        meta.file_type().is_file() && meta.len() >= self.max_bytes
    }
}

/// Whether `rel` sits under one of the folders another root looks after. A folder watched
/// on its own only claims its direct children, so the tree below it is still this root's.
pub fn under_excluded(rel: &[u8], excluded: &[ExcludedDir]) -> bool {
    excluded.iter().any(|(dir, recursive)| {
        rel.len() > dir.len()
            && rel.starts_with(dir)
            && rel[dir.len()] == b'/'
            && (*recursive || !rel[dir.len() + 1..].contains(&b'/'))
    })
}

/// Whether the whole tree below `rel` belongs to another root, so the walk need not open
/// it at all.
fn dir_fully_excluded(rel: &[u8], excluded: &[ExcludedDir]) -> bool {
    excluded.iter().any(|(dir, recursive)| {
        *recursive
            && (rel == dir.as_slice()
                || (rel.len() > dir.len() && rel.starts_with(dir) && rel[dir.len()] == b'/'))
    })
}

/// One `lstat` answer, kept so the size rule and the hashing pass can share it instead of
/// each paying its own syscall.
#[derive(Debug, Clone)]
pub enum MetaOf {
    Present(std::fs::Metadata),
    /// Nothing at the path: a recorded file that has been deleted.
    Absent,
    /// The `lstat` itself failed, with the reason to show.
    Failed(String),
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

/// How long another process's temp index must have gone untouched before the sweep takes it.
const STALE_TEMP_INDEX: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Remove this process's own temp index and any *stale* one left by a process that died
/// (Phase 5 deliverable 2a).
///
/// Ours goes unconditionally - a leftover from our own crashed fold, which `read-tree` would
/// replace anyway. Another process's goes only when it has not been modified for an hour,
/// because there is no liveness probe here: a pid on this machine says nothing (it may have
/// been reused, and the file may belong to a container's pid namespace), and adding a probe
/// would mean a new dependency for a file that costs nothing to leave lying. On Windows
/// nothing but ours is ever removed. A failure at any step is silence: a temp index we could
/// not delete is litter, never a reason to fail an open.
fn sweep_temp_indexes(repo_dir: &Path, ours: &Path) {
    let _ = std::fs::remove_file(ours);
    if cfg!(windows) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !paths::is_temp_index_name(name) {
            continue;
        }
        let path = entry.path();
        if path == ours {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|m| now.duration_since(m).map_err(std::io::Error::other))
            .is_ok_and(|age| age >= STALE_TEMP_INDEX);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
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
        sweep_temp_indexes(&paths.repo_dir, &paths.index_tmp);

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
        let metas: Vec<MetaOf> = rels.iter().map(|rel| self.lstat(rel)).collect();
        self.hash_paths_with(rels, &metas)
    }

    /// One `symlink_metadata`, never following a link.
    pub fn lstat(&self, rel: &[u8]) -> MetaOf {
        let full = self.git.root().join(OsStr::from_bytes(rel));
        match std::fs::symlink_metadata(&full) {
            Ok(m) => MetaOf::Present(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => MetaOf::Absent,
            Err(e) => MetaOf::Failed(e.to_string()),
        }
    }

    /// [`Store::hash_paths`] over `lstat` answers the caller already has. A watched folder
    /// has to know every candidate's size before it decides which ones to read, so it does
    /// that pass itself and hands the results here rather than paying for them twice.
    pub fn hash_paths_with(&self, rels: &[Vec<u8>], metas: &[MetaOf]) -> Vec<Current> {
        assert_eq!(rels.len(), metas.len(), "one lstat answer per path");
        let mut out: Vec<Option<Current>> = vec![None; rels.len()];
        let mut batch: Vec<usize> = Vec::new();
        let metas: Vec<Option<std::fs::Metadata>> = metas
            .iter()
            .enumerate()
            .map(|(i, m)| match m {
                MetaOf::Present(m) => {
                    if m.file_type().is_file() && Self::batchable(&rels[i]) {
                        batch.push(i);
                    }
                    Some(m.clone())
                }
                MetaOf::Absent => {
                    out[i] = Some(Current::Absent);
                    None
                }
                MetaOf::Failed(reason) => {
                    out[i] = Some(Current::Unhashable(reason.clone()));
                    None
                }
            })
            .collect();
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

    /// `hash-object -w --path=<rel> --stdin`: the oid a **scan** will compute for `bytes`
    /// once they are the content of `rel` (Phase 8 deliverable 1).
    ///
    /// The same runner and environment as [`Store::hash_path`] — `GIT_DIR=<store>`,
    /// `GIT_WORK_TREE=<root>`, cwd = root — so `rel`'s attributes (`text=auto`, `eol`, a
    /// clean filter) act exactly as they do on a scanned file. That is the whole point:
    /// a save must know the post-write oid **before** it writes anything (design review
    /// F1), because `restore::write_bytes`'s `before_rename` hook takes no arguments and
    /// cannot reach the temp file, and a fresh read after the rename would hash whatever
    /// an agent put there in the meantime. `--path` is what separates this from
    /// [`Store::hash_bytes`], which deliberately passes no path because its input is
    /// already-canonical blob content.
    ///
    /// A draft root has no `.gitattributes`, so the two calls agree there trivially.
    ///
    /// **`-w` writes the object, so a save that is then refused leaves an orphan blob in
    /// the store** (verifier (a) F6). That is deliberate and it is the same class of orphan
    /// a scan's `hash_path -w` leaves for content nobody ever accepts: the store is never
    /// gc'd (§11), the object is small, and the alternative — hashing without `-w` and
    /// writing the object after the rename — would mean a fresh read of a file an agent may
    /// have touched in between, which is exactly the window design review F1 closed. Do not
    /// "fix" this into a post-rename read.
    pub fn hash_bytes_as(&self, rel: &[u8], bytes: &[u8]) -> Result<Oid, StoreError> {
        let mut path_arg = OsString::from("--path=");
        path_arg.push(OsStr::from_bytes(rel));
        let out = self.git.run_stdin(
            None,
            &[
                OsString::from("hash-object"),
                OsString::from("-w"),
                path_arg,
                OsString::from("--stdin"),
            ],
            bytes,
        )?;
        Oid::parse(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| StoreError::Other("hash-object --path --stdin printed no oid".into()))
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

    /// A tree of the current disk content (a watched folder's first sight): a temp index,
    /// `update-index --add -z --stdin` over the file list (which writes the blobs), then
    /// `write-tree`. Never `git add`. Skips `.git` entries and nested repositories.
    ///
    /// `scope` says how much of the folder this is: without `recursive` only the files
    /// directly inside it, and either way nothing at or above `scope.max_bytes` — a large
    /// file is never copied into the store, which is what keeps a first sight of a folder
    /// holding a disk image cheap. `excluded` are the folders other roots look after.
    pub fn tree_of_disk(
        &self,
        scope: &DraftScope,
        excluded: &[ExcludedDir],
    ) -> Result<Oid, StoreError> {
        let mut files: Vec<Vec<u8>> = Vec::new();
        walk_files(self.git.root(), Path::new(""), scope, excluded, &mut files)?;
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

fn walk_files(
    root: &Path,
    rel: &Path,
    scope: &DraftScope,
    excluded: &[ExcludedDir],
    out: &mut Vec<Vec<u8>>,
) -> Result<(), StoreError> {
    let dir = root.join(rel);
    let entries = std::fs::read_dir(&dir).map_err(|e| io_err(&dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let child_rel = rel.join(&name);
        let child_bytes = child_rel.as_os_str().as_bytes().to_vec();
        // `DirEntry::metadata` is an `lstat` on unix: a symlink's own size, never its
        // target's.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            // A nested repository is another root (D9), and so is a folder watched with
            // its whole tree: neither is opened here.
            if !scope.recursive
                || dir_fully_excluded(&child_bytes, excluded)
                || root.join(&child_rel).join(".git").exists()
            {
                continue;
            }
            walk_files(root, &child_rel, scope, excluded, out)?;
        } else if (ft.is_file() || ft.is_symlink())
            && scope.admits(&child_bytes, &meta)
            && !under_excluded(&child_bytes, excluded)
        {
            out.push(child_bytes);
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
        open_git_at(repo, state, &RepoPaths::under(state.join("repo")))
    }

    fn open_git_at(repo: &FixtureRepo, state: &TempDir, paths: &RepoPaths) -> (Store, RepoGit) {
        let env = fixture_env(repo, state);
        let rg = RepoGit::new(&env, repo.path());
        let config = rg.config_list().unwrap();
        let facts = RepoFacts::read(&rg, &config).unwrap();
        let (store, notices) =
            Store::open(&env, repo.path(), RootKind::Git, paths, Some(&facts)).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        (store, rg)
    }

    /// Set `path`'s mtime `secs` into the past, so a sweep sees an aged file without a sleep.
    fn age(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// The sweep takes our own temp index and a *dead* process's aged one, and nothing else:
    /// not a temp index another live lastcall touched a minute ago, and not the persistent
    /// index or its tree stamp, which share the `index` stem (deliverable 2a).
    #[test]
    fn store_sweeps_only_our_temp_index_and_an_hour_old_orphan() {
        let dir = TempDir::new("lc-sweep");
        let repo_dir = dir.mkdir("repo");
        let paths = RepoPaths::under(repo_dir.clone());
        // A pid that is not ours; the sweep never probes liveness, only the mtime.
        let dead = repo_dir.join(paths::temp_index_name(999_999));
        let live = repo_dir.join(paths::temp_index_name(std::process::id() + 1));
        for f in [
            &paths.index_tmp,
            &dead,
            &live,
            &paths.index,
            &paths.index_tree,
        ] {
            std::fs::write(f, b"x").unwrap();
        }
        age(&dead, 60 * 60 + 5);
        age(&live, 60);
        // The persistent index is old too: age alone must not be enough to take a file.
        age(&paths.index, 60 * 60 * 24);

        sweep_temp_indexes(&repo_dir, &paths.index_tmp);

        assert!(!paths.index_tmp.exists(), "ours goes unconditionally");
        assert!(!dead.exists(), "an hour-old orphan goes");
        assert!(live.exists(), "a temp index touched a minute ago stays");
        assert!(paths.index.exists(), "the persistent index is never swept");
        assert!(paths.index_tree.exists(), "nor its tree stamp");
    }

    /// And `Store::open` is where it runs.
    #[test]
    fn store_open_sweeps_a_stale_temp_index_left_by_a_dead_process() {
        let repo = FixtureRepo::new("sweep").unwrap();
        let state = TempDir::new("lc-sweep-open");
        let repo_dir = state.mkdir("repo");
        let paths = RepoPaths::under(repo_dir.clone());
        let dead = repo_dir.join(paths::temp_index_name(999_998));
        std::fs::write(&dead, b"x").unwrap();
        age(&dead, 60 * 60 + 5);

        let (store, _) = open_git_at(&repo, &state, &paths);

        assert!(!dead.exists(), "the stale temp index is gone after open");
        assert!(
            store
                .index_tmp
                .ends_with(paths::temp_index_name(std::process::id())),
            "the store scans through its own process's temp index: {}",
            store.index_tmp.display()
        );
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
        let tree = store
            .tree_of_disk(&DraftScope::tree(u64::MAX), &[])
            .unwrap();
        let entries = store.ls_tree(&tree).unwrap();
        let paths: Vec<&[u8]> = entries.keys().map(Vec::as_slice).collect();
        assert_eq!(paths, vec![&b"a.md"[..], &b"l"[..], &b"sub/b.md"[..]]);
        assert_eq!(entries[&b"l"[..]].0, Mode::Symlink);
        assert!(store.exists(&entries[&b"a.md"[..]].1), "blobs were written");
    }

    /// Amendment v1.13 R1 and R2: a folder watched on its own records the files directly
    /// inside it, a folder watched with its whole tree records the tree, and neither
    /// records a file at or above the size at which reading stops. The boundary is
    /// at-or-above: `max_bytes - 1` is recorded and `max_bytes` is not.
    #[test]
    fn store_tree_of_disk_under_each_scope_and_the_size_boundary() {
        let dir = TempDir::new("lc-draft-scope");
        let root = dir.mkdir("notes");
        let max = 4096u64;
        dir.write("notes/a.md", "a\n");
        std::fs::write(root.join("just_under.bin"), vec![b'x'; max as usize - 1]).unwrap();
        std::fs::write(root.join("at_limit.bin"), vec![b'x'; max as usize]).unwrap();
        dir.write("notes/sub/b.md", "b\n");
        std::fs::write(root.join("sub/big.bin"), vec![b'x'; max as usize]).unwrap();
        // A symlink's own size is the text of the link, so a link to a large file is read.
        std::os::unix::fs::symlink("at_limit.bin", root.join("link_to_big")).unwrap();
        let state = dir.mkdir("state");
        let env = Env::empty(dir.path()).with_home(dir.mkdir("home"));
        let paths = RepoPaths::under(state.join("repo"));
        let (store, _) = Store::open(&env, &root, RootKind::Draft, &paths, None).unwrap();
        let recorded = |scope: &DraftScope, excluded: &[ExcludedDir]| -> Vec<String> {
            let tree = store.tree_of_disk(scope, excluded).unwrap();
            store
                .ls_tree(&tree)
                .unwrap()
                .keys()
                .map(|k| String::from_utf8_lossy(k).into_owned())
                .collect()
        };

        assert_eq!(
            recorded(&DraftScope::plain(max), &[]),
            vec![
                "a.md".to_owned(),
                "just_under.bin".to_owned(),
                "link_to_big".to_owned(),
            ],
            "one folder, and nothing at the limit"
        );
        assert_eq!(
            recorded(&DraftScope::tree(max), &[]),
            vec![
                "a.md".to_owned(),
                "just_under.bin".to_owned(),
                "link_to_big".to_owned(),
                "sub/b.md".to_owned(),
            ],
            "the tree, still nothing at the limit"
        );
        // A folder another root looks after is not this root's, whether that root reads its
        // tree or only its direct files.
        assert!(
            !recorded(&DraftScope::tree(max), &[(b"sub".to_vec(), true)])
                .contains(&"sub/b.md".to_owned())
        );
        assert!(
            !recorded(&DraftScope::tree(max), &[(b"sub".to_vec(), false)])
                .contains(&"sub/b.md".to_owned())
        );
    }

    /// The size predicate on its own, at the boundary and on the shapes that are not
    /// regular files.
    #[test]
    fn store_draft_scope_predicate_at_the_boundary() {
        let dir = TempDir::new("lc-scope-pred");
        let root = dir.mkdir("r");
        std::fs::write(root.join("under"), vec![b'x'; 99]).unwrap();
        std::fs::write(root.join("at"), vec![b'x'; 100]).unwrap();
        dir.mkdir("r/d");
        std::os::unix::fs::symlink("at", root.join("l")).unwrap();
        let meta = |n: &str| std::fs::symlink_metadata(root.join(n)).unwrap();
        let scope = DraftScope::plain(100);
        assert!(!scope.too_big(&meta("under")));
        assert!(scope.too_big(&meta("at")), "at the limit is not read");
        assert!(!scope.too_big(&meta("d")), "a folder is not a file");
        assert!(
            !scope.too_big(&meta("l")),
            "a symlink carries its link text"
        );

        assert!(scope.admits(b"a.md", &meta("under")));
        assert!(
            !scope.admits(b"sub/a.md", &meta("under")),
            "one folder only"
        );
        assert!(!scope.admits(b"a.md", &meta("at")));
        assert!(DraftScope::tree(100).admits(b"sub/a.md", &meta("under")));
        // Shape and size are separate questions: what a folder lists is not what it reads.
        assert!(scope.admits_shape(b"a.md"));
        assert!(!scope.admits_shape(b"sub/a.md"));
        assert!(DraftScope::tree(100).admits_shape(b"sub/deep/a.md"));
    }

    /// A folder watched on its own claims its direct children only, so the tree below it
    /// still belongs to the root above (design review F2, scenario F8).
    #[test]
    fn store_under_excluded_reads_the_recursive_flag() {
        let recursive = [(b"research".to_vec(), true)];
        let plain = [(b"research".to_vec(), false)];
        assert!(under_excluded(b"research/b.md", &recursive));
        assert!(under_excluded(b"research/deep/c.md", &recursive));
        assert!(under_excluded(b"research/b.md", &plain));
        assert!(
            !under_excluded(b"research/deep/c.md", &plain),
            "the inner root reads nothing below its own folder"
        );
        assert!(!under_excluded(b"research", &plain), "the folder itself");
        assert!(
            !under_excluded(b"researchy/b.md", &plain),
            "prefix, not path"
        );
    }

    /// The contract deliverable 1 rests on: the oid a save records **before** it writes is
    /// the oid the next scan computes **after** it wrote (design review F1).
    ///
    /// Checked where the two could diverge — under `text=auto` and under an explicit
    /// `eol=crlf`, both of which make git store something other than the bytes on disk —
    /// and on a draft root, which has no attributes at all.
    #[test]
    fn store_hash_bytes_as_equals_hash_path_after_write() {
        let mut repo = FixtureRepo::new("hash-as").unwrap();
        repo.write(".gitattributes", "* text=auto\ncrlf.txt eol=crlf\n");
        repo.commit("attrs").unwrap();
        let state = TempDir::new("lc-store");
        let (store, _) = open_git(&repo, &state);
        let cases: [(&[u8], &[u8]); 4] = [
            (b"plain.txt", b"a\nb\nc\n"),
            (b"auto.txt", b"a\r\nb\r\nc\r\n"),
            (b"crlf.txt", b"x\r\ny\r\n"),
            (b"noeol.txt", b"no trailing newline"),
        ];
        for (rel, bytes) in cases {
            let before = store.hash_bytes_as(rel, bytes).unwrap();
            crate::restore::write_bytes(&store, rel, bytes, Some(Mode::Regular), &mut || Ok(()))
                .unwrap();
            assert_eq!(
                std::fs::read(repo.path().join(OsStr::from_bytes(rel))).unwrap(),
                bytes,
                "{}: the bytes go down verbatim",
                String::from_utf8_lossy(rel)
            );
            match store.hash_path(rel) {
                Current::Present { oid, .. } => assert_eq!(
                    oid,
                    before,
                    "{}: the pre-write oid is the post-write scan's oid",
                    String::from_utf8_lossy(rel)
                ),
                other => panic!("{other:?}"),
            }
        }

        // A draft root: no `.gitattributes` anywhere, so `--path` changes nothing — the
        // save path still goes through the same call and must still agree.
        let dir = TempDir::new("lc-draft-hash");
        let root = dir.mkdir("notes");
        let dstate = dir.mkdir("state");
        let env = Env::empty(dir.path()).with_home(dir.mkdir("home"));
        let dpaths = RepoPaths::under(dstate.join("repo"));
        let (draft, _) = Store::open(&env, &root, RootKind::Draft, &dpaths, None).unwrap();
        let bytes = b"one\r\ntwo\n";
        let before = draft.hash_bytes_as(b"n.md", bytes).unwrap();
        crate::restore::write_bytes(&draft, b"n.md", bytes, Some(Mode::Regular), &mut || Ok(()))
            .unwrap();
        assert_eq!(std::fs::read(root.join("n.md")).unwrap(), bytes);
        match draft.hash_path(b"n.md") {
            Current::Present { oid, .. } => assert_eq!(oid, before),
            other => panic!("{other:?}"),
        }
    }
}

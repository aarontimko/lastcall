//! The single spawn site for `git` (docs/spec/91-phase2-kickoff.md, deliverable 1).
//!
//! Exactly one module in the engine spawns `git`; the gate grep
//! `rg -n 'Command::new\("git"\)' crates/lastcall-engine/src` must match this file only.
//!
//! Two runners, both built on the injected [`Env`]:
//!
//! - [`StoreGit`] — every command against **our** private store: cwd is the root (the
//!   `text=auto` rule, §6.4), `GIT_DIR=<store>`, `GIT_WORK_TREE=<root>` and `GIT_INDEX_FILE`
//!   set explicitly on every call.
//! - [`RepoGit`] — read-only inspection of the **user's** repository with the three variables
//!   removed. Its command set is a closed allowlist ([`RepoGit::allowed`]); the unit tests are
//!   the guard. Never `status`, `diff`, `add`, `update-index`, `checkout`, `stash`, `gc`: the
//!   user's index is theirs (skip-worktree bits are *read* from it, D6).
//!
//! Both: `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`; the process env is
//! inherited (production honors the user's global config), every `GIT_*` / `XDG_*` / `HOME`
//! present in the injected `Env` overlays it (tests null the global config that way), and
//! **after** the overlay the repository-location variables are scrubbed so a git hook shell
//! or an agent shell that exports them can never redirect the store or the inspection.
//! Paths are bytes end to end; every path-producing command runs with `-z`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::env::Env;

/// Variables scrubbed from every child after the `Env` overlay. Any of them would redirect
/// where git reads or writes (`GIT_OBJECT_DIRECTORY` alone would redirect the store's
/// writes).
pub const SCRUBBED_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_INDEX_VERSION",
];

/// The minimum git version every plumbing flag we use exists in (`--path-format=absolute`
/// is 2.31).
pub const MIN_GIT_VERSION: (u32, u32) = (2, 31);

/// A git object id (40 hex chars for SHA-1 repositories).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Oid(String);

impl Oid {
    /// The all-zero id git prints for "absent" sides.
    pub fn zero() -> Self {
        Self("0".repeat(40))
    }

    /// Parse a hex id; refuses anything that is not lowercase hex of length 40 or 64.
    pub fn parse(s: &str) -> Option<Self> {
        let ok = (s.len() == 40 || s.len() == 64)
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        ok.then(|| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0.bytes().all(|b| b == b'0')
    }
}

impl std::fmt::Display for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Debug for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Oid({})", &self.0[..self.0.len().min(12)])
    }
}

/// A git file mode as the index stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// `100644`
    Regular,
    /// `100755`
    Executable,
    /// `120000`
    Symlink,
    /// `160000` (a submodule entry)
    Gitlink,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Regular => "100644",
            Mode::Executable => "100755",
            Mode::Symlink => "120000",
            Mode::Gitlink => "160000",
        }
    }

    /// Parse git's octal spelling. `0` / `000000` (an absent side) is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "100644" => Some(Mode::Regular),
            "100755" => Some(Mode::Executable),
            "120000" => Some(Mode::Symlink),
            "160000" => Some(Mode::Gitlink),
            _ => None,
        }
    }
}

impl serde::Serialize for Mode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Mode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Mode::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("bad git mode {s:?}")))
    }
}

/// A `git` failure. Every variant carries the argv and cwd it ran with.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("cannot spawn git {argv:?} in {}: {message}", cwd.display())]
    Spawn {
        argv: Vec<String>,
        cwd: PathBuf,
        message: String,
    },
    #[error("git {argv:?} in {} exited {}: {stderr}", cwd.display(), status.map_or("by signal".to_string(), |s| s.to_string()))]
    Failed {
        argv: Vec<String>,
        cwd: PathBuf,
        status: Option<i32>,
        stderr: String,
    },
    #[error("git {argv:?}: refused, not in RepoGit's read-only allowlist")]
    NotAllowed { argv: Vec<String> },
    #[error("git {argv:?}: unparsable output: {message}")]
    Parse { argv: Vec<String>, message: String },
}

/// Raw output of a finished `git` child (status not interpreted).
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<i32>,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

fn argv_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The base command: inherit the process env, overlay the injected `Env`'s `GIT_*` /
/// `XDG_*` / `HOME`, pin the three safety variables, then scrub the repository-location
/// variables. Callers set their own `GIT_DIR` etc. afterwards.
fn base_command(env: &Env, cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    for (k, v) in env.vars() {
        if k.starts_with("GIT_") || k.starts_with("XDG_") || k == "HOME" {
            cmd.env(k, v);
        }
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for k in SCRUBBED_VARS {
        cmd.env_remove(k);
    }
    cmd
}

fn run_command(
    mut cmd: Command,
    args: &[OsString],
    stdin: Option<&[u8]>,
) -> Result<GitOutput, GitError> {
    use std::io::Write;
    let cwd = cmd
        .get_current_dir()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().map_err(|e| GitError::Spawn {
        argv: argv_strings(args),
        cwd: cwd.clone(),
        message: e.to_string(),
    })?;
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        // A child that exits early (bad argument) closes the pipe; that is reported through
        // the exit status, not here.
        let _ = pipe.write_all(bytes);
        drop(pipe);
    }
    let out = child.wait_with_output().map_err(|e| GitError::Spawn {
        argv: argv_strings(args),
        cwd,
        message: e.to_string(),
    })?;
    Ok(GitOutput {
        stdout: out.stdout,
        stderr: out.stderr,
        status: out.status.code(),
    })
}

fn require_success(out: GitOutput, args: &[OsString], cwd: &Path) -> Result<Vec<u8>, GitError> {
    if out.success() {
        Ok(out.stdout)
    } else {
        Err(GitError::Failed {
            argv: argv_strings(args),
            cwd: cwd.to_path_buf(),
            status: out.status,
            stderr: out.stderr_lossy(),
        })
    }
}

fn to_argv<S: AsRef<OsStr>>(args: &[S]) -> Vec<OsString> {
    args.iter().map(|a| a.as_ref().to_os_string()).collect()
}

/// `git --version` → `(major, minor, patch)`.
pub fn git_version(env: &Env) -> Result<(u32, u32, u32), GitError> {
    let args = to_argv(&["--version"]);
    let cwd = env.cwd().to_path_buf();
    let out = run_command(base_command(env, &cwd), &args, None)?;
    let text = String::from_utf8_lossy(&require_success(out, &args, &cwd)?).into_owned();
    parse_git_version(&text).ok_or_else(|| GitError::Parse {
        argv: argv_strings(&args),
        message: format!("cannot parse {text:?}"),
    })
}

/// Parse `git version 2.37.1 (Apple Git-137.1)` / `git version 2.45.0.windows.1`.
pub fn parse_git_version(text: &str) -> Option<(u32, u32, u32)> {
    let rest = text.trim().strip_prefix("git version ")?;
    let token = rest.split_whitespace().next()?;
    let mut parts = token.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether a parsed version satisfies [`MIN_GIT_VERSION`].
pub fn version_supported((major, minor, _): (u32, u32, u32)) -> bool {
    (major, minor) >= MIN_GIT_VERSION
}

// ---------------------------------------------------------------------------------------
// StoreGit
// ---------------------------------------------------------------------------------------

/// Plumbing against our private store, with the user's root as the work tree.
#[derive(Debug)]
pub struct StoreGit {
    env: Env,
    root: PathBuf,
    store: PathBuf,
    index: PathBuf,
    excludes_file: Option<PathBuf>,
    hash_object_calls: AtomicU64,
}

impl StoreGit {
    /// `store` is the bare repository, `index` the private index file (`<repo>/index`).
    pub fn new(env: &Env, root: &Path, store: &Path, index: &Path) -> Self {
        Self {
            env: env.clone(),
            root: root.to_path_buf(),
            store: store.to_path_buf(),
            index: index.to_path_buf(),
            excludes_file: None,
            hash_object_calls: AtomicU64::new(0),
        }
    }

    /// The user's effective `core.excludesfile`, passed as `-c` so `--exclude-standard`
    /// under our `GIT_DIR` honors it even when it comes from a per-repo config.
    #[must_use]
    pub fn with_excludes_file(mut self, path: Option<PathBuf>) -> Self {
        self.excludes_file = path;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store_dir(&self) -> &Path {
        &self.store
    }

    pub fn index_path(&self) -> &Path {
        &self.index
    }

    /// Debug counter: `hash-object` invocations so far (the scan's stat-cache test).
    pub fn hash_object_calls(&self) -> u64 {
        self.hash_object_calls.load(Ordering::Relaxed)
    }

    fn command(&self, index: &Path) -> Command {
        let mut cmd = base_command(&self.env, &self.root);
        cmd.env("GIT_DIR", &self.store)
            .env("GIT_WORK_TREE", &self.root)
            .env("GIT_INDEX_FILE", index);
        cmd
    }

    fn full_args<S: AsRef<OsStr>>(&self, args: &[S]) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(args.len() + 2);
        if let Some(p) = &self.excludes_file {
            let mut v = OsString::from("core.excludesfile=");
            v.push(p);
            argv.push(OsString::from("-c"));
            argv.push(v);
        }
        argv.extend(to_argv(args));
        argv
    }

    /// Run against the private index; success required.
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Vec<u8>, GitError> {
        self.run_with(None, args, None)
    }

    /// Run against `index` (a temp index); success required.
    pub fn run_with_index<S: AsRef<OsStr>>(
        &self,
        index: &Path,
        args: &[S],
    ) -> Result<Vec<u8>, GitError> {
        self.run_with(Some(index), args, None)
    }

    /// Run with bytes on stdin; success required.
    pub fn run_stdin<S: AsRef<OsStr>>(
        &self,
        index: Option<&Path>,
        args: &[S],
        stdin: &[u8],
    ) -> Result<Vec<u8>, GitError> {
        self.run_with(index, args, Some(stdin))
    }

    fn run_with<S: AsRef<OsStr>>(
        &self,
        index: Option<&Path>,
        args: &[S],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>, GitError> {
        let out = self.run_raw(index, args, stdin)?;
        let argv = self.full_args(args);
        require_success(out, &argv, &self.root)
    }

    /// Run and return the raw output whatever the exit status (`update-index --refresh` is
    /// non-zero when files differ).
    pub fn run_raw<S: AsRef<OsStr>>(
        &self,
        index: Option<&Path>,
        args: &[S],
        stdin: Option<&[u8]>,
    ) -> Result<GitOutput, GitError> {
        let argv = self.full_args(args);
        if args.first().is_some_and(|a| a.as_ref() == "hash-object") {
            self.hash_object_calls.fetch_add(1, Ordering::Relaxed);
        }
        run_command(self.command(index.unwrap_or(&self.index)), &argv, stdin)
    }

    /// `git init -q --bare` at `store` (an associated function: there is no store yet).
    pub fn init_bare(env: &Env, store: &Path) -> Result<(), GitError> {
        let cwd = store
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut store_arg = OsString::new();
        store_arg.push(store);
        let args = vec![
            OsString::from("init"),
            OsString::from("-q"),
            OsString::from("--bare"),
            store_arg,
        ];
        let out = run_command(base_command(env, &cwd), &args, None)?;
        require_success(out, &args, &cwd).map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------
// RepoGit
// ---------------------------------------------------------------------------------------

/// Read-only inspection of the user's repository.
#[derive(Debug, Clone)]
pub struct RepoGit {
    env: Env,
    root: PathBuf,
}

impl RepoGit {
    pub fn new(env: &Env, root: &Path) -> Self {
        Self {
            env: env.clone(),
            root: root.to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The closed allowlist. `args[0]` must be the subcommand (no global options), and the
    /// stateful forms of otherwise-read-only commands are refused:
    /// `ls-files` only with `-v`, `-u` or `--stage`/`-s`; `config` only `--get`;
    /// `symbolic-ref` with exactly one ref (the two-ref form writes); `log` only with a
    /// `--format`; `worktree` only `list --porcelain`.
    pub fn allowed<S: AsRef<OsStr>>(args: &[S]) -> bool {
        let strs: Vec<String> = args
            .iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect();
        let Some(sub) = strs.first() else {
            return false;
        };
        let rest = &strs[1..];
        match sub.as_str() {
            "--version" => rest.is_empty(),
            "rev-parse" | "rev-list" | "merge-base" | "diff-tree" | "cat-file" | "for-each-ref" => {
                true
            }
            "symbolic-ref" => rest.iter().filter(|a| !a.starts_with('-')).count() == 1,
            "config" => rest.first().is_some_and(|a| a == "--get"),
            "ls-files" => rest
                .iter()
                .any(|a| a == "-v" || a == "-u" || a == "--stage" || a == "-s"),
            "log" => rest.iter().any(|a| a.starts_with("--format")),
            "worktree" => {
                rest.first().is_some_and(|a| a == "list") && rest.iter().any(|a| a == "--porcelain")
            }
            _ => false,
        }
    }

    fn command(&self) -> Command {
        // base_command already scrubbed GIT_DIR / GIT_WORK_TREE / GIT_INDEX_FILE.
        base_command(&self.env, &self.root)
    }

    /// Run an allowlisted command; success required.
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Vec<u8>, GitError> {
        let out = self.run_raw(args, None)?;
        require_success(out, &to_argv(args), &self.root)
    }

    /// Run an allowlisted command with stdin; success required.
    pub fn run_stdin<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        stdin: &[u8],
    ) -> Result<Vec<u8>, GitError> {
        let out = self.run_raw(args, Some(stdin))?;
        require_success(out, &to_argv(args), &self.root)
    }

    /// Run an allowlisted command and return the raw output whatever the status
    /// (`rev-parse -q --verify` exits 1 for "no such ref").
    pub fn run_raw<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        stdin: Option<&[u8]>,
    ) -> Result<GitOutput, GitError> {
        let argv = to_argv(args);
        if !Self::allowed(args) {
            return Err(GitError::NotAllowed {
                argv: argv_strings(&argv),
            });
        }
        run_command(self.command(), &argv, stdin)
    }

    /// `rev-parse -q --verify <spec>` → `Some(oid)` when it resolves.
    pub fn rev_parse_verify(&self, spec: &str) -> Result<Option<Oid>, GitError> {
        let out = self.run_raw(&["rev-parse", "-q", "--verify", spec], None)?;
        if !out.success() {
            return Ok(None);
        }
        Ok(Oid::parse(&out.stdout_trimmed()))
    }

    /// `rev-parse --path-format=absolute --git-path <name>`.
    pub fn git_path(&self, name: &str) -> Result<PathBuf, GitError> {
        let out = self.run(&["rev-parse", "--path-format=absolute", "--git-path", name])?;
        Ok(PathBuf::from(String::from_utf8_lossy(&out).trim()))
    }

    /// `config --get <key>` → `None` when unset (exit 1).
    pub fn config_get(&self, key: &str) -> Result<Option<String>, GitError> {
        let out = self.run_raw(&["config", "--get", key], None)?;
        match out.status {
            Some(0) => Ok(Some(out.stdout_trimmed())),
            Some(1) => Ok(None),
            _ => Err(GitError::Failed {
                argv: vec!["config".into(), "--get".into(), key.into()],
                cwd: self.root.clone(),
                status: out.status,
                stderr: out.stderr_lossy(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------
// NUL parsers
// ---------------------------------------------------------------------------------------

/// Split NUL-terminated records, dropping the empty tail.
pub fn split_nul(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// One `diff-files -z` / `diff-tree -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub old_mode: Option<Mode>,
    pub new_mode: Option<Mode>,
    pub old_oid: Oid,
    pub new_oid: Oid,
    /// `M`, `D`, `A`, `T`, `U`, `R`, `C` (the letter only).
    pub status: char,
    pub path: Vec<u8>,
    /// The destination for `R`/`C` records.
    pub dest: Option<Vec<u8>>,
}

/// Parse `:<omode> <nmode> <ooid> <noid> <status>\0<path>\0[<dest>\0]` records.
pub fn parse_diff_z(bytes: &[u8]) -> Result<Vec<DiffEntry>, String> {
    let fields = split_nul(bytes);
    let mut out = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let head = std::str::from_utf8(fields[i]).map_err(|e| e.to_string())?;
        let head = head
            .strip_prefix(':')
            .ok_or_else(|| format!("expected ':' record, got {head:?}"))?;
        let parts: Vec<&str> = head.split(' ').collect();
        if parts.len() != 5 {
            return Err(format!("bad diff record {head:?}"));
        }
        let status_char = parts[4]
            .chars()
            .next()
            .ok_or_else(|| "empty status".to_string())?;
        let path = fields
            .get(i + 1)
            .ok_or_else(|| "missing path".to_string())?
            .to_vec();
        let mut consumed = 2;
        let dest = if matches!(status_char, 'R' | 'C') {
            consumed = 3;
            Some(
                fields
                    .get(i + 2)
                    .ok_or_else(|| "missing rename destination".to_string())?
                    .to_vec(),
            )
        } else {
            None
        };
        out.push(DiffEntry {
            old_mode: Mode::parse(parts[0]),
            new_mode: Mode::parse(parts[1]),
            old_oid: Oid::parse(parts[2]).ok_or_else(|| format!("bad oid {:?}", parts[2]))?,
            new_oid: Oid::parse(parts[3]).ok_or_else(|| format!("bad oid {:?}", parts[3]))?,
            status: status_char,
            path,
            dest,
        });
        i += consumed;
    }
    Ok(out)
}

/// One `ls-tree -r -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: Mode,
    pub oid: Oid,
    pub path: Vec<u8>,
}

/// Parse `<mode> <type> <oid>\t<path>\0` records.
pub fn parse_ls_tree_z(bytes: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut out = Vec::new();
    for rec in split_nul(bytes) {
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| "ls-tree record without tab".to_string())?;
        let head = std::str::from_utf8(&rec[..tab]).map_err(|e| e.to_string())?;
        let parts: Vec<&str> = head.split(' ').collect();
        if parts.len() != 3 {
            return Err(format!("bad ls-tree record {head:?}"));
        }
        let Some(mode) = Mode::parse(parts[0]) else {
            // `-r` never lists trees; anything else unknown is skipped, never invented.
            continue;
        };
        out.push(TreeEntry {
            mode,
            oid: Oid::parse(parts[2]).ok_or_else(|| format!("bad oid {:?}", parts[2]))?,
            path: rec[tab + 1..].to_vec(),
        });
    }
    Ok(out)
}

/// One `ls-files --stage -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageEntry {
    pub mode: Option<Mode>,
    pub oid: Oid,
    pub stage: u8,
    pub path: Vec<u8>,
}

/// Parse `<mode> <oid> <stage>\t<path>\0` records.
pub fn parse_ls_files_stage_z(bytes: &[u8]) -> Result<Vec<StageEntry>, String> {
    let mut out = Vec::new();
    for rec in split_nul(bytes) {
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| "ls-files --stage record without tab".to_string())?;
        let head = std::str::from_utf8(&rec[..tab]).map_err(|e| e.to_string())?;
        let parts: Vec<&str> = head.split(' ').collect();
        if parts.len() != 3 {
            return Err(format!("bad ls-files --stage record {head:?}"));
        }
        out.push(StageEntry {
            mode: Mode::parse(parts[0]),
            oid: Oid::parse(parts[1]).ok_or_else(|| format!("bad oid {:?}", parts[1]))?,
            stage: parts[2]
                .parse()
                .map_err(|_| format!("bad stage {:?}", parts[2]))?,
            path: rec[tab + 1..].to_vec(),
        });
    }
    Ok(out)
}

/// Parse `ls-files -v -z` records: `<tag> <path>\0` where the tag is one letter (`H`
/// cached, `S` skip-worktree, `M` unmerged, `h`/`s`/`m` assume-unchanged variants, `?`
/// other, `R` removed, `C` modified, `K` to be killed).
pub fn parse_ls_files_v_z(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    split_nul(bytes)
        .into_iter()
        .filter_map(|rec| {
            if rec.len() >= 2 && rec[1] == b' ' {
                Some((rec[0], rec[2..].to_vec()))
            } else {
                None
            }
        })
        .collect()
}

/// One `cat-file --batch-check` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchCheck {
    Present { oid: Oid, kind: String, size: u64 },
    Missing(String),
    Ambiguous(String),
}

/// Parse `--batch-check` lines: `<oid> <type> <size>`, `<obj> missing`, `<obj> ambiguous`.
pub fn parse_batch_check(bytes: &[u8]) -> Vec<BatchCheck> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split(' ').collect();
            match parts.as_slice() {
                [obj, "missing"] => BatchCheck::Missing((*obj).to_string()),
                [obj, "ambiguous"] => BatchCheck::Ambiguous((*obj).to_string()),
                [oid, kind, size] => match (Oid::parse(oid), size.parse::<u64>()) {
                    (Some(oid), Ok(size)) => BatchCheck::Present {
                        oid,
                        kind: (*kind).to_string(),
                        size,
                    },
                    _ => BatchCheck::Missing(line.to_string()),
                },
                _ => BatchCheck::Missing(line.to_string()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_repo_allowlist_accepts_the_read_only_forms() {
        for args in [
            vec!["rev-parse", "HEAD"],
            vec![
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
            vec!["symbolic-ref", "-q", "--short", "HEAD"],
            vec!["config", "--get", "user.email"],
            vec!["ls-files", "-v", "-z"],
            vec!["ls-files", "-u", "-z"],
            vec!["ls-files", "--stage", "-z"],
            vec!["rev-list", "a..b", "--not", "--remotes"],
            vec!["merge-base", "--is-ancestor", "a", "b"],
            vec!["log", "--cc", "--name-only", "-z", "--format=%H", "a..b"],
            vec![
                "diff-tree",
                "--no-commit-id",
                "-r",
                "--name-only",
                "--cc",
                "x",
            ],
            vec!["cat-file", "--batch-check"],
            vec!["for-each-ref", "--format=%(refname)"],
            vec!["worktree", "list", "--porcelain"],
            vec!["--version"],
        ] {
            assert!(RepoGit::allowed(&args), "{args:?} must be allowed");
        }
    }

    #[test]
    fn git_repo_allowlist_refuses_anything_that_can_write_the_users_repo() {
        for args in [
            vec!["status", "--porcelain=v2", "-z"],
            vec!["diff", "--name-only"],
            vec!["add", "-N", "x"],
            vec!["update-index", "--refresh"],
            vec!["checkout", "main"],
            vec!["stash"],
            vec!["gc"],
            vec!["commit", "-m", "x"],
            vec!["reset", "--hard"],
            vec!["ls-files"],
            vec!["ls-files", "--others", "-z"],
            vec!["config", "user.email", "x@y"],
            vec!["config", "--unset", "user.email"],
            vec!["symbolic-ref", "HEAD", "refs/heads/x"],
            vec!["log"],
            vec!["worktree", "add", "../x"],
            vec!["worktree", "prune"],
            vec!["-c", "core.x=y", "rev-parse", "HEAD"],
            vec!["-C", "/elsewhere", "rev-parse", "HEAD"],
            vec![],
        ] {
            assert!(!RepoGit::allowed(&args), "{args:?} must be refused");
        }
    }

    #[test]
    fn git_repo_run_refuses_before_spawning() {
        let git = RepoGit::new(&Env::empty("/nonexistent-cwd"), Path::new("/nonexistent"));
        let err = git.run(&["status"]).unwrap_err();
        assert!(matches!(err, GitError::NotAllowed { .. }), "{err}");
    }

    #[test]
    fn git_base_command_overlays_env_and_scrubs_locations() {
        let env = Env::empty("/work")
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_DIR", "/leaked")
            .with_var("GIT_OBJECT_DIRECTORY", "/leaked-objects")
            .with_var("XDG_CONFIG_HOME", "/xdg")
            .with_var("HOME", "/home/t")
            .with_var("LASTCALL_STATE_DIR", "/not-overlaid");
        let cmd = base_command(&env, Path::new("/work"));
        let envs: std::collections::BTreeMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs["GIT_CONFIG_GLOBAL"].as_deref(), Some("/dev/null"));
        assert_eq!(envs["XDG_CONFIG_HOME"].as_deref(), Some("/xdg"));
        assert_eq!(envs["HOME"].as_deref(), Some("/home/t"));
        assert_eq!(envs["GIT_OPTIONAL_LOCKS"].as_deref(), Some("0"));
        assert_eq!(envs["GIT_TERMINAL_PROMPT"].as_deref(), Some("0"));
        assert_eq!(envs["LC_ALL"].as_deref(), Some("C"));
        assert!(!envs.contains_key("LASTCALL_STATE_DIR"));
        for k in SCRUBBED_VARS {
            assert_eq!(envs.get(*k), Some(&None), "{k} must be removed");
        }
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/work")));
    }

    #[test]
    fn git_store_command_sets_the_three_locations_explicitly() {
        let env = Env::empty("/work").with_var("GIT_DIR", "/leaked");
        let store = StoreGit::new(
            &env,
            Path::new("/root"),
            Path::new("/state/store"),
            Path::new("/state/index"),
        )
        .with_excludes_file(Some(PathBuf::from("/home/u/.gitignore_global")));
        let cmd = store.command(Path::new("/state/index.tmp"));
        let envs: std::collections::BTreeMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs["GIT_DIR"].as_deref(), Some("/state/store"));
        assert_eq!(envs["GIT_WORK_TREE"].as_deref(), Some("/root"));
        assert_eq!(envs["GIT_INDEX_FILE"].as_deref(), Some("/state/index.tmp"));
        assert_eq!(envs.get("GIT_OBJECT_DIRECTORY"), Some(&None));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/root")));
        let argv = store.full_args(&["ls-files", "--others", "-z"]);
        assert_eq!(
            argv,
            vec![
                OsString::from("-c"),
                OsString::from("core.excludesfile=/home/u/.gitignore_global"),
                OsString::from("ls-files"),
                OsString::from("--others"),
                OsString::from("-z"),
            ]
        );
    }

    #[test]
    fn git_parse_version_handles_apple_and_plain_forms() {
        assert_eq!(
            parse_git_version("git version 2.37.1 (Apple Git-137.1)\n"),
            Some((2, 37, 1))
        );
        assert_eq!(parse_git_version("git version 2.45.0"), Some((2, 45, 0)));
        assert_eq!(
            parse_git_version("git version 2.45.0.windows.1"),
            Some((2, 45, 0))
        );
        assert_eq!(parse_git_version("git version 2.31"), Some((2, 31, 0)));
        assert_eq!(parse_git_version("nonsense"), None);
        assert!(version_supported((2, 31, 0)));
        assert!(version_supported((2, 37, 1)));
        assert!(!version_supported((2, 30, 9)));
        assert!(!version_supported((1, 99, 0)));
    }

    #[test]
    fn git_parse_diff_z_records_including_rename() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let z = "0".repeat(40);
        let bytes = format!(
            ":100644 100644 {a} {b} M\0f1\0:100644 000000 {a} {z} D\0dir/f 2\0:100644 100644 {a} {b} R090\0old\0new\0"
        );
        let entries = parse_diff_z(bytes.as_bytes()).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].status, 'M');
        assert_eq!(entries[0].path, b"f1");
        assert_eq!(entries[0].old_mode, Some(Mode::Regular));
        assert_eq!(entries[1].status, 'D');
        assert_eq!(entries[1].new_mode, None);
        assert!(entries[1].new_oid.is_zero());
        assert_eq!(entries[1].path, b"dir/f 2");
        assert_eq!(entries[2].status, 'R');
        assert_eq!(entries[2].path, b"old");
        assert_eq!(entries[2].dest.as_deref(), Some(&b"new"[..]));
        assert!(parse_diff_z(b"garbage\0x\0").is_err());
        assert!(parse_diff_z(b"").unwrap().is_empty());
    }

    #[test]
    fn git_parse_ls_tree_and_stage_and_v_records() {
        let a = "a".repeat(40);
        let tree = format!("100644 blob {a}\tdocs/résumé draft.md\0120000 blob {a}\tlink\0");
        let entries = parse_ls_tree_z(tree.as_bytes()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mode, Mode::Regular);
        assert_eq!(entries[0].path, "docs/résumé draft.md".as_bytes());
        assert_eq!(entries[1].mode, Mode::Symlink);

        let stage = format!("100644 {a} 0\tf1\0100644 {a} 2\tc\0100644 {a} 3\tc\0");
        let entries = parse_ls_files_stage_z(stage.as_bytes()).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].stage, 2);
        assert_eq!(entries[2].path, b"c");

        let v = parse_ls_files_v_z(b"H f1\0S other/o\0M c\0");
        assert_eq!(
            v,
            vec![
                (b'H', b"f1".to_vec()),
                (b'S', b"other/o".to_vec()),
                (b'M', b"c".to_vec())
            ]
        );
    }

    #[test]
    fn git_parse_batch_check_answers() {
        let a = "a".repeat(40);
        let out = parse_batch_check(
            format!("{a} blob 12\ndeadbeef missing\nHEAD:x missing\n").as_bytes(),
        );
        assert_eq!(
            out[0],
            BatchCheck::Present {
                oid: Oid::parse(&a).unwrap(),
                kind: "blob".into(),
                size: 12
            }
        );
        assert_eq!(out[1], BatchCheck::Missing("deadbeef".into()));
        assert_eq!(out[2], BatchCheck::Missing("HEAD:x".into()));
    }

    #[test]
    fn git_oid_and_mode_parse_and_render() {
        assert!(Oid::parse(&"0".repeat(40)).unwrap().is_zero());
        assert!(Oid::parse("xyz").is_none());
        assert!(Oid::parse(&"A".repeat(40)).is_none(), "uppercase refused");
        assert_eq!(Mode::parse("100755"), Some(Mode::Executable));
        assert_eq!(Mode::parse("000000"), None);
        assert_eq!(Mode::parse("0"), None);
        assert_eq!(Mode::Symlink.as_str(), "120000");
        let json = serde_json::to_string(&Mode::Executable).unwrap();
        assert_eq!(json, "\"100755\"");
        let back: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Mode::Executable);
        assert!(serde_json::from_str::<Mode>("\"777\"").is_err());
    }

    #[test]
    fn git_split_nul_drops_only_the_empty_tail() {
        assert_eq!(split_nul(b"a\0b\0"), vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(split_nul(b"a\0\0b\0"), vec![&b"a"[..], &b""[..], &b"b"[..]]);
        assert!(split_nul(b"").is_empty());
    }
}

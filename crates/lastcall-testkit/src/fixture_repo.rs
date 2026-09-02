//! Deterministic fixture repositories (kickoff deliverable 9): a port of the Phase 0 harness's
//! `mk_repo` / `coworker_push` (`scripts/harness/scenarios.sh:9-19`).
//!
//! `FixtureRepo::new(name)` creates a temp working repo on branch `main` with a local bare
//! `origin`, commits and pushes the harness's three seed files, and fixes everything that
//! would otherwise make hashes drift: `GIT_CONFIG_GLOBAL=/dev/null`,
//! `GIT_CONFIG_SYSTEM=/dev/null`, a fixed author/committer identity, and
//! `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE` that advance by one deterministic minute per commit
//! from a fixed epoch. Two builds with the same script therefore yield identical `HEAD`
//! hashes (`tests/test_integration_fixture_repo.rs`). Phase 2's git-plumbing tests build on it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tmp::TempDir;

/// The seed files `mk_repo` commits (`scripts/harness/scenarios.sh:12`).
pub const SEED_FILES: &[(&str, &str)] = &[
    ("f1", "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n"),
    ("f2", "b\n"),
    ("f3", "c\n"),
];

/// Fixed identity for the repo's own commits.
pub const AUTHOR_NAME: &str = "Me";
pub const AUTHOR_EMAIL: &str = "me@example.com";
/// The coworker who pushes to `origin` from a separate clone.
pub const COWORKER_NAME: &str = "Coworker";
pub const COWORKER_EMAIL: &str = "coworker@example.com";

/// Unix epoch seconds of the first commit; each later commit is one minute later.
const BASE_EPOCH: u64 = 1_767_225_600; // 2026-01-01T00:00:00Z

/// A `git` failure with the command and its stderr.
#[derive(Debug, thiserror::Error)]
#[error("git {args:?} in {cwd}: {stderr}")]
pub struct GitError {
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub stderr: String,
}

/// A working repo with a local bare `origin`, removed on drop.
pub struct FixtureRepo {
    _dir: TempDir,
    name: String,
    work: PathBuf,
    origin: PathBuf,
    /// Commits made so far across the repo and the coworker clone; drives the fixed dates.
    ticks: u64,
}

impl std::fmt::Debug for FixtureRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixtureRepo")
            .field("name", &self.name)
            .field("work", &self.work)
            .field("origin", &self.origin)
            .finish()
    }
}

impl FixtureRepo {
    /// `mk_repo NAME`: init `NAME` (branch `main`) and bare `NAME.git`, add the remote, commit
    /// the seed files, push `-u origin main`.
    pub fn new(name: &str) -> Result<Self, GitError> {
        let dir = TempDir::new("lc-fixture");
        let work = dir.join(name);
        let origin = dir.join(format!("{name}.git"));
        let mut repo = Self {
            _dir: dir,
            name: name.to_string(),
            work,
            origin,
            ticks: 0,
        };
        std::fs::create_dir_all(&repo.work).map_err(|e| repo.io_error(&e))?;
        repo.git_in(&repo.work.clone(), &["init", "-q", "-b", "main"])?;
        repo.git_in(
            &repo.work.clone(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                repo.origin.to_str().unwrap(),
            ],
        )?;
        let origin = repo.origin.to_string_lossy().to_string();
        repo.git(&["remote", "add", "origin", &origin])?;
        // Local identity: the engine's upstream classifier reads `user.email` from the repo
        // config (docs/spec/00-spec.md §6.4), and the child env below is not what it sees.
        repo.git(&["config", "user.name", AUTHOR_NAME])?;
        repo.git(&["config", "user.email", AUTHOR_EMAIL])?;
        repo.commit_files(SEED_FILES, "init")?;
        repo.git(&["push", "-q", "-u", "origin", "main"])?;
        Ok(repo)
    }

    /// The working repo.
    pub fn path(&self) -> &Path {
        &self.work
    }

    /// The bare `origin`.
    pub fn origin(&self) -> &Path {
        &self.origin
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Write files (creating parents), `git add -A`, commit as the fixed author. Returns the
    /// commit hash.
    pub fn commit_files(
        &mut self,
        files: &[(&str, &str)],
        message: &str,
    ) -> Result<String, GitError> {
        for (path, contents) in files {
            let full = self.work.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).map_err(|e| self.io_error(&e))?;
            }
            std::fs::write(&full, contents).map_err(|e| self.io_error(&e))?;
        }
        self.git(&["add", "-A"])?;
        let date = self.next_date();
        self.git_with_identity(
            &self.work.clone(),
            AUTHOR_NAME,
            AUTHOR_EMAIL,
            &date,
            &["commit", "-q", "-m", message],
        )?;
        self.head()
    }

    /// `coworker_push N`: from a fresh clone of `origin`, write `u1..uN` (`up1..upN`), commit
    /// as the coworker, push to `origin main`. Returns the pushed commit hash. The working repo
    /// is untouched (fetch/rebase is the scenario's choice).
    pub fn coworker_push(&mut self, n_files: usize) -> Result<String, GitError> {
        self.coworker_push_prefixed(n_files, "u")
    }

    /// `coworker_push N PREFIX`.
    pub fn coworker_push_prefixed(
        &mut self,
        n_files: usize,
        prefix: &str,
    ) -> Result<String, GitError> {
        let clone = self._dir.join(format!("{}.cw", self.name));
        let _ = std::fs::remove_dir_all(&clone);
        let origin = self.origin.to_string_lossy().to_string();
        self.git_in(
            self._dir.path(),
            &["clone", "-q", &origin, clone.to_str().unwrap()],
        )?;
        for i in 1..=n_files {
            std::fs::write(clone.join(format!("{prefix}{i}")), format!("up{i}\n"))
                .map_err(|e| self.io_error(&e))?;
        }
        self.git_in(&clone, &["add", "-A"])?;
        let date = self.next_date();
        self.git_with_identity(
            &clone,
            COWORKER_NAME,
            COWORKER_EMAIL,
            &date,
            &["commit", "-q", "-m", &format!("coworker {n_files} files")],
        )?;
        self.git_in(&clone, &["push", "-q", "origin", "main"])?;
        let sha = self.git_in(&clone, &["rev-parse", "HEAD"])?;
        Ok(sha.trim().to_string())
    }

    /// `git rev-parse HEAD` of the working repo.
    pub fn head(&self) -> Result<String, GitError> {
        Ok(self.git(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// `git rev-parse origin/main` as the working repo sees it (after a fetch).
    pub fn origin_main(&self) -> Result<String, GitError> {
        Ok(self
            .git_in(&self.origin, &["rev-parse", "main"])?
            .trim()
            .to_string())
    }

    /// Run `git` in the working repo with the fixed environment.
    pub fn git(&self, args: &[&str]) -> Result<String, GitError> {
        self.git_in(&self.work, args)
    }

    /// Write a file under the working repo (creating parents) without committing.
    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let full = self.work.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&full, contents).expect("write fixture file");
        full
    }

    /// Remove a file (or an empty directory) under the working repo.
    pub fn remove(&self, rel: &str) {
        let full = self.work.join(rel);
        if full.is_dir() {
            std::fs::remove_dir(&full).expect("remove fixture dir");
        } else {
            std::fs::remove_file(&full).expect("remove fixture file");
        }
    }

    /// Create a symlink at `rel` pointing at `target` (link text as given).
    pub fn symlink(&self, target: &str, rel: &str) {
        std::os::unix::fs::symlink(target, self.work.join(rel)).expect("symlink");
    }

    /// `chmod +x` (or `-x`) a file under the working repo.
    pub fn chmod_x(&self, rel: &str, executable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let full = self.work.join(rel);
        let mut perms = std::fs::metadata(&full).expect("stat").permissions();
        let mode = if executable {
            perms.mode() | 0o111
        } else {
            perms.mode() & !0o111
        };
        perms.set_mode(mode);
        std::fs::set_permissions(&full, perms).expect("chmod");
    }

    /// `git checkout -q -b <branch>`.
    pub fn checkout_b(&self, branch: &str) -> Result<(), GitError> {
        self.git(&["checkout", "-q", "-b", branch]).map(|_| ())
    }

    /// `git checkout -q <target>`.
    pub fn checkout(&self, target: &str) -> Result<(), GitError> {
        self.git(&["checkout", "-q", target]).map(|_| ())
    }

    /// The engine's injected environment for this fixture: the same git isolation as the
    /// fixture's own commands (`GIT_CONFIG_GLOBAL=/dev/null` etc.), `LASTCALL_STATE_DIR` set
    /// to `state_dir`, a home directory inside the fixture's temp dir, and `cwd` = the
    /// working repo. No test may reach the real home.
    pub fn engine_env(&self, state_dir: &Path) -> lastcall_engine::env::Env {
        let home = self._dir.join("home");
        std::fs::create_dir_all(&home).expect("create fixture home");
        lastcall_engine::env::Env::empty(self.work.clone())
            .with_home(home)
            .with_var("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_var("GIT_CONFIG_SYSTEM", "/dev/null")
            .with_var("GIT_CONFIG_NOSYSTEM", "1")
            .with_var("LASTCALL_STATE_DIR", state_dir.to_string_lossy())
    }

    /// The fixture's temp dir (the parent of the working repo and `origin`).
    pub fn parent_dir(&self) -> &Path {
        self._dir.path()
    }

    fn next_date(&mut self) -> String {
        let date = format!("{} +0000", BASE_EPOCH + self.ticks * 60);
        self.ticks += 1;
        date
    }

    fn git_in(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        // Non-commit commands still get the fixed identity; dates matter only for commits.
        let date = format!("{} +0000", BASE_EPOCH + self.ticks * 60);
        self.git_with_identity(cwd, AUTHOR_NAME, AUTHOR_EMAIL, &date, args)
    }

    fn git_with_identity(
        &self,
        cwd: &Path,
        name: &str,
        email: &str,
        date: &str,
        args: &[&str],
    ) -> Result<String, GitError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .map_err(|e| GitError {
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.to_path_buf(),
                stderr: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(GitError {
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.to_path_buf(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn io_error(&self, err: &std::io::Error) -> GitError {
        GitError {
            args: vec!["<fs>".to_string()],
            cwd: self.work.clone(),
            stderr: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_repo_seeds_three_files_and_pushes_to_origin() {
        let repo = FixtureRepo::new("seed").unwrap();
        for (name, contents) in SEED_FILES {
            assert_eq!(
                std::fs::read_to_string(repo.path().join(name)).unwrap(),
                *contents
            );
        }
        assert_eq!(repo.head().unwrap(), repo.origin_main().unwrap());
        let remote = repo.git(&["remote", "get-url", "origin"]).unwrap();
        assert_eq!(remote.trim(), repo.origin().to_string_lossy());
        let branch = repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(branch.trim(), "main");
    }

    #[test]
    fn fixture_repo_coworker_push_advances_origin_only() {
        let mut repo = FixtureRepo::new("cw").unwrap();
        let before = repo.head().unwrap();
        let pushed = repo.coworker_push(2).unwrap();
        assert_ne!(pushed, before);
        assert_eq!(repo.origin_main().unwrap(), pushed);
        assert_eq!(
            repo.head().unwrap(),
            before,
            "the working repo is untouched"
        );
        repo.git(&["fetch", "-q"]).unwrap();
        let author = repo
            .git(&["log", "-1", "--format=%ae", "origin/main"])
            .unwrap();
        assert_eq!(author.trim(), COWORKER_EMAIL);
        let files = repo
            .git(&["ls-tree", "--name-only", "origin/main"])
            .unwrap();
        assert!(files.contains("u1") && files.contains("u2"), "{files}");
    }
}

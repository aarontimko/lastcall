//! The isolated real-herdr spawner (docs/spec/00-spec.md §5.10; kickoff deliverable 7).
//!
//! A port of herdr's own test recipe (`tests/api_ping.rs:17-160` and the PID registry in
//! `tests/support/mod.rs`, Apache-2.0; see NOTICE) using **safe wrappers only**
//! (`unsafe_code = "forbid"` at the workspace level): `portable_pty::Child::{kill, try_wait}`
//! for the owned child and `nix::sys::signal::kill` for the PID registry. Never `libc`.
//!
//! Isolation, per spawn:
//! - a fresh base dir `/tmp/lc-<pid>-<nanos>/` (never `$TMPDIR`: macOS caps `sun_path` at 104
//!   bytes; the socket path is asserted to be under 100 bytes);
//! - `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, `HOME` inside it, an explicit `HERDR_SOCKET_PATH`,
//!   `SHELL=/bin/sh`, and **every inherited `HERDR_*` variable removed** (our own shell may be
//!   inside herdr — a child that inherited `HERDR_SOCKET_PATH` would nest into the sponsor's
//!   live session);
//! - `<XDG_CONFIG_HOME>/herdr/config.toml` containing `onboarding = false` written before
//!   spawning (herdr's default is onboarding-on and `ensure_default_workspace` returns early in
//!   onboarding mode, `src/app/mod.rs:1248-1254`, so the snapshot would have zero workspaces).
//!
//! Process hygiene: kill-on-drop with a bounded `try_wait` poll, a registry of spawned PIDs
//! with a kill-on-panic hook, and a matcher that refuses to kill any PID not in the registry
//! and additionally checks `ps -o comm= -p <pid>` against the spawned binary path (macOS has
//! no `/proc`).
//!
//! The binary path comes only from `LASTCALL_TEST_HERDR_BIN` (set by `just
//! test-integration-herdr`); never from a `herdr` on `PATH`, which could be the user's live
//! install.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// The environment variable naming the pinned herdr binary.
pub const BIN_ENV: &str = "LASTCALL_TEST_HERDR_BIN";

/// The skip line, written with `stderr().write_all` (libtest swallows `eprintln!` of passing
/// tests) and echoed at the `just` level too.
pub const SKIP_MESSAGE: &str =
    "SKIP: LASTCALL_TEST_HERDR_BIN unset (run: just test-integration-herdr)";

/// The herdr binary from `LASTCALL_TEST_HERDR_BIN`, if set and non-empty.
pub fn herdr_bin_from_env() -> Option<PathBuf> {
    herdr_bin_from(std::env::var_os(BIN_ENV))
}

/// The filtering behind [`herdr_bin_from_env`]: empty means unset.
pub fn herdr_bin_from(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Write the skip notice so it is visible even for a passing test.
pub fn write_skip_notice() {
    let mut err = std::io::stderr();
    let _ = err.write_all(SKIP_MESSAGE.as_bytes());
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

/// `/tmp/lc-<pid>-<nanos>` (herdr uses `/tmp/hapi-<pid>-<nanos>` for the same reason).
pub fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/lc-{}-{nanos}", std::process::id()))
}

/// The per-spawn directories and the explicit socket path.
#[derive(Debug, Clone)]
pub struct HerdrIsolation {
    pub base: PathBuf,
    pub config_home: PathBuf,
    pub runtime_dir: PathBuf,
    pub home: PathBuf,
    /// Private `XDG_STATE_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME`: herdr's `state_dir()`
    /// prefers `XDG_STATE_HOME` (plugins, manifest cache, announcements), so an inherited
    /// value would let a test server write into the sponsor's real state (review F1).
    pub state_home: PathBuf,
    pub data_home: PathBuf,
    pub cache_home: PathBuf,
    pub socket_path: PathBuf,
}

impl HerdrIsolation {
    /// Create the directories and the `onboarding = false` config.
    pub fn create() -> std::io::Result<Self> {
        let base = unique_test_dir();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let home = base.join("home");
        let state_home = base.join("state");
        let data_home = base.join("data");
        let cache_home = base.join("cache");
        let socket_path = runtime_dir.join("herdr.sock");
        let len = socket_path.as_os_str().len();
        if len >= 100 {
            return Err(std::io::Error::other(format!(
                "socket path {} is {len} bytes; must stay under 100 (macOS sun_path cap)",
                socket_path.display()
            )));
        }
        std::fs::create_dir_all(config_home.join("herdr"))?;
        std::fs::create_dir_all(&runtime_dir)?;
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&state_home)?;
        std::fs::create_dir_all(&data_home)?;
        std::fs::create_dir_all(&cache_home)?;
        std::fs::write(
            config_home.join("herdr/config.toml"),
            "onboarding = false\n",
        )?;
        Ok(Self {
            base,
            config_home,
            runtime_dir,
            home,
            state_home,
            data_home,
            cache_home,
            socket_path,
        })
    }
}

/// A running `herdr server` inside a PTY. Killed on drop.
pub struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    bin: PathBuf,
    isolation: HerdrIsolation,
}

impl std::fmt::Debug for SpawnedHerdr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedHerdr")
            .field("pid", &self.pid)
            .field("bin", &self.bin)
            .field("socket_path", &self.isolation.socket_path)
            .finish()
    }
}

impl SpawnedHerdr {
    /// Spawn `<bin> server` with the §5.10 isolation.
    pub fn spawn(bin: &Path) -> std::io::Result<Self> {
        let bin = std::fs::canonicalize(bin)?;
        let isolation = HerdrIsolation::create()?;
        register_runtime_dir(&isolation.base);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(std::io::Error::other)?;

        let mut cmd = CommandBuilder::new(&bin);
        cmd.arg("server");
        // Clear every inherited HERDR_* first: our own shell may be inside herdr.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("XDG_CONFIG_HOME", &isolation.config_home);
        cmd.env("XDG_RUNTIME_DIR", &isolation.runtime_dir);
        cmd.env("HOME", &isolation.home);
        cmd.env("XDG_STATE_HOME", &isolation.state_home);
        cmd.env("XDG_DATA_HOME", &isolation.data_home);
        cmd.env("XDG_CACHE_HOME", &isolation.cache_home);
        cmd.env("HERDR_SOCKET_PATH", &isolation.socket_path);
        cmd.env("SHELL", "/bin/sh");
        cmd.cwd(&isolation.base);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        let pid = child.process_id();
        register_spawned_pid(pid, &bin);

        Ok(Self {
            _master: pair.master,
            child,
            pid,
            bin,
            isolation,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.isolation.socket_path
    }

    pub fn isolation(&self) -> &HerdrIsolation {
        &self.isolation
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Poll `exists && connect` every 25 ms up to `timeout` — never sleep-and-hope.
    pub fn wait_for_socket(&mut self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(std::io::Error::other(format!(
                    "herdr exited before its socket appeared: {status:?}"
                )));
            }
            let path = &self.isolation.socket_path;
            if path.exists() && UnixStream::connect(path).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "socket did not appear at {} within {timeout:?}",
                self.isolation.socket_path.display()
            ),
        ))
    }

    /// Whether the PID matcher would accept this child (for tests of the matcher itself).
    pub fn matcher_accepts(&self) -> bool {
        self.pid
            .is_some_and(|pid| process_matches_binary(pid, &self.bin))
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        unregister_spawned_pid(self.pid);
        unregister_runtime_dir(&self.isolation.base);
        let _ = std::fs::remove_dir_all(&self.isolation.base);
    }
}

// ---------------------------------------------------------------------------------------
// PID registry with kill-on-panic (safe wrappers only)
// ---------------------------------------------------------------------------------------

static PID_REGISTRY: OnceLock<Mutex<HashMap<u32, PathBuf>>> = OnceLock::new();
static RUNTIME_DIRS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
static HOOKS: Once = Once::new();

fn registry() -> &'static Mutex<HashMap<u32, PathBuf>> {
    PID_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_dirs() -> &'static Mutex<Vec<PathBuf>> {
    RUNTIME_DIRS.get_or_init(|| Mutex::new(Vec::new()))
}

fn ensure_cleanup_hooks() {
    HOOKS.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            kill_all_registered();
            previous(info);
        }));
    });
}

/// Shared with [`crate::pty_tui`]: every PTY child of this process is registered here.
pub(crate) fn register_spawned_pid(pid: Option<u32>, bin: &Path) {
    let Some(pid) = pid else {
        return;
    };
    ensure_cleanup_hooks();
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pid, bin.to_path_buf());
}

pub(crate) fn unregister_spawned_pid(pid: Option<u32>) {
    if let Some(pid) = pid {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid);
    }
}

fn register_runtime_dir(dir: &Path) {
    ensure_cleanup_hooks();
    runtime_dirs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(dir.to_path_buf());
}

fn unregister_runtime_dir(dir: &Path) {
    runtime_dirs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|d| d != dir);
}

/// `ps -o comm= -p <pid>` compared against the spawned binary: equal to the full path, or
/// (some `ps` builds print only the name) equal to its file name.
pub fn process_matches_binary(pid: u32, bin: &Path) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let comm = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if comm.is_empty() {
        return false;
    }
    let comm_path = Path::new(&comm);
    let same_path = std::fs::canonicalize(comm_path)
        .map(|c| c == bin)
        .unwrap_or(comm_path == bin);
    let same_name = comm_path.file_name().is_some() && comm_path.file_name() == bin.file_name();
    // A name match alone is not enough: a recycled PID could belong to the sponsor's live
    // `herdr`. Every process we may kill was spawned by this process, so also require the
    // parent pid to be ours (review F2).
    (same_path || same_name) && parent_pid(pid) == Some(std::process::id())
}

/// `ps -o ppid= -p <pid>`; `None` when the process is gone or `ps` fails.
pub fn parent_pid(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Kill one registered PID, refusing anything the matcher does not recognise.
/// Returns whether a signal was sent.
pub fn kill_registered(pid: u32) -> bool {
    let bin = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&pid)
        .cloned();
    let Some(bin) = bin else {
        return false; // never kill a stranger
    };
    if !process_matches_binary(pid, &bin) {
        return false;
    }
    let target = nix::unistd::Pid::from_raw(pid as i32);
    nix::sys::signal::kill(target, nix::sys::signal::Signal::SIGKILL).is_ok()
}

/// Kill every registered PID (the panic hook) and remove their runtime dirs.
pub fn kill_all_registered() {
    let pids: Vec<u32> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect();
    for pid in pids {
        kill_registered(pid);
    }
    let dirs: Vec<PathBuf> = runtime_dirs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_spawn_isolation_dirs_are_short_and_carry_onboarding_off() {
        let iso = HerdrIsolation::create().unwrap();
        assert!(iso.socket_path.as_os_str().len() < 100);
        assert!(iso.socket_path.to_string_lossy().starts_with("/tmp/lc-"));
        let config = std::fs::read_to_string(iso.config_home.join("herdr/config.toml")).unwrap();
        assert_eq!(config, "onboarding = false\n");
        assert!(iso.home.is_dir());
        std::fs::remove_dir_all(&iso.base).unwrap();
    }

    #[test]
    fn herdr_spawn_matcher_refuses_unregistered_and_mismatched_pids() {
        // Our own PID is not registered: never killed.
        assert!(!kill_registered(std::process::id()));
        // Our own PID does not match a fictional binary path.
        assert!(!process_matches_binary(
            std::process::id(),
            Path::new("/nonexistent/herdr")
        ));
        // A PID that certainly does not exist.
        assert!(!process_matches_binary(u32::MAX - 1, Path::new("/bin/sh")));
    }

    #[test]
    fn herdr_spawn_bin_env_must_be_non_empty() {
        // Unset and empty both mean "no binary"; anything else is the path verbatim.
        assert_eq!(herdr_bin_from(None), None);
        assert_eq!(herdr_bin_from(Some(std::ffi::OsString::new())), None);
        assert_eq!(
            herdr_bin_from(Some(std::ffi::OsString::from("/x/herdr"))),
            Some(PathBuf::from("/x/herdr"))
        );
    }

    #[test]
    fn herdr_spawn_matcher_requires_our_own_child() {
        // A real child of ours whose name matches: accepted. The same name under a
        // different parent (our own parent process): refused.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert_eq!(parent_pid(pid), Some(std::process::id()));
        assert!(process_matches_binary(pid, Path::new("/bin/sleep")));
        assert!(!process_matches_binary(pid, Path::new("/bin/zsh")));
        let _ = child.kill();
        let _ = child.wait();
        let me = std::process::id();
        assert!(!process_matches_binary(
            me,
            Path::new(&std::env::current_exe().unwrap())
        ));
    }
}

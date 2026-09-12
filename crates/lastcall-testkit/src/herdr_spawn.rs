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
//! - `<XDG_CONFIG_HOME>/herdr/config.toml` containing [`HERDR_TEST_CONFIG`] written before
//!   spawning: `onboarding = false` (herdr's default is onboarding-on and
//!   `ensure_default_workspace` returns early in onboarding mode, `src/app/mod.rs:1248-1254`,
//!   so the snapshot would have zero workspaces) plus `version_check` and `manifest_check`
//!   off, which is what keeps this tier off the network;
//! - [`HERDR_OFFLINE_ENV`] in the spawn environment, pointing the agent-detection manifest
//!   catalogue at a closed loopback port.
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
use std::sync::{Arc, Mutex, Once, OnceLock};
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

/// `/tmp/lc-<pid>-<nanos>-<n>` (herdr uses `/tmp/hapi-<pid>-<nanos>` for the same reason).
///
/// The counter is not decoration. macOS reports `SystemTime::now()` at **microsecond**
/// granularity (every value here ends in `000`), so two tests in one binary that spawn a
/// server at the same moment used to get the *same* base — and therefore the same
/// `HERDR_SOCKET_PATH`, at which point the second server found the first's socket and exited
/// with `error: herdr server is already running`. The process-wide counter makes two
/// isolations in one process distinct whatever the clock does, and the pid keeps two
/// processes apart.
pub fn unique_test_dir() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PathBuf::from(format!("/tmp/lc-{}-{nanos}-{n}", std::process::id()))
}

/// The config a spawned herdr is given, and the reason for each line.
///
/// `onboarding = false` skips the first-run wizard, which would otherwise hold the server.
/// The other two are why nothing here reaches the network: herdr's `version_check` and
/// `manifest_check` both default to **true**, and a server started with them on curls
/// `herdr.dev/latest.json` and `herdr.dev/agent-detection/index.toml` in the background,
/// twice per spawn, from a suite the boundaries say never reaches the network (verifier (a)
/// F2, which counted sixteen such calls in one run of the real-herdr subset).
///
/// The section header is load-bearing: both switches live under `[update]` in herdr's own
/// config (`UpdateConfig { channel, version_check, manifest_check }`, and the template the
/// binary embeds prints them under `[update]`). Written flat at the top level they are
/// unknown keys, which herdr ignores in silence: a first run of this fix with flat keys
/// still logged `https://herdr.dev/latest.json` once per spawn.
pub const HERDR_TEST_CONFIG: &str =
    "onboarding = false\n\n[update]\nversion_check = false\nmanifest_check = false\n";

/// Belt and braces beside [`HERDR_TEST_CONFIG`]: a manifest catalogue on a closed loopback
/// port, so a herdr that ever ignores `manifest_check` fails to connect instead of leaving
/// the machine. Port 1 answers nothing.
pub const HERDR_OFFLINE_ENV: &[(&str, &str)] = &[(
    "HERDR_AGENT_DETECTION_MANIFEST_CATALOG_URL",
    "http://127.0.0.1:1/",
)];

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
    /// Create the directories and the [`HERDR_TEST_CONFIG`] config.
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
        // `create_dir`, not `create_dir_all`: an already-existing base means two isolations
        // collided, and a shared socket path is exactly the failure this must not reach.
        std::fs::create_dir(&base).map_err(|e| {
            std::io::Error::new(e.kind(), format!("isolation base {}: {e}", base.display()))
        })?;
        std::fs::create_dir_all(config_home.join("herdr"))?;
        std::fs::create_dir_all(&runtime_dir)?;
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&state_home)?;
        std::fs::create_dir_all(&data_home)?;
        std::fs::create_dir_all(&cache_home)?;
        std::fs::write(config_home.join("herdr/config.toml"), HERDR_TEST_CONFIG)?;
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

    /// Every environment variable a herdr process (server or CLI) must be given so it can
    /// only ever see this isolation.
    ///
    /// One list, used by [`SpawnedHerdr::spawn`], [`SpawnedHerdr::respawn`] and
    /// [`SpawnedHerdr::stop_server`], so a restart cannot drift onto the sponsor's real
    /// socket or config (kickoff "Operational rules": sacred and untouchable).
    pub fn env_pairs(&self) -> Vec<(&'static str, &Path)> {
        vec![
            ("XDG_CONFIG_HOME", self.config_home.as_path()),
            ("XDG_RUNTIME_DIR", self.runtime_dir.as_path()),
            ("HOME", self.home.as_path()),
            ("XDG_STATE_HOME", self.state_home.as_path()),
            ("XDG_DATA_HOME", self.data_home.as_path()),
            ("XDG_CACHE_HOME", self.cache_home.as_path()),
            ("HERDR_SOCKET_PATH", self.socket_path.as_path()),
        ]
    }

    /// Refuse to start or stop anything that is not inside a private `/tmp/lc-…` base.
    ///
    /// Called before **every** spawn, respawn and `herdr server stop`: `herdr server stop`
    /// reads `HERDR_SOCKET_PATH` (`src/session.rs:173-181` at v0.8.2), so a leaked or
    /// hand-built isolation would stop the sponsor's live session instead of ours.
    pub fn assert_isolated(&self) -> std::io::Result<()> {
        let base = self.base.to_string_lossy().to_string();
        if !base.starts_with("/tmp/lc-") {
            return Err(std::io::Error::other(format!(
                "isolation base {base} is not a private /tmp/lc-… dir; refusing to touch it"
            )));
        }
        for (name, path) in self.env_pairs() {
            if !path.starts_with(&self.base) {
                return Err(std::io::Error::other(format!(
                    "{name}={} escapes the isolation base {base}; refusing",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

/// A running `herdr server` inside a PTY. Killed on drop.
pub struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    /// What this server last wrote to its pty (the drain thread keeps it).
    tail: PtyTail,
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
        let (master, child, pid, tail) = spawn_server_child(&bin, &isolation)?;
        Ok(Self {
            _master: master,
            child,
            pid,
            tail,
            bin,
            isolation,
        })
    }

    /// Stop this server the way a user would — `herdr server stop` **over its own isolated
    /// socket** — so the socket file is gone deterministically when the call returns
    /// (`stop_socket_with_timeout` waits for it, `src/session.rs:260-296` at v0.8.2).
    ///
    /// A `SIGKILL` would leave the stale socket file behind and the client would see a
    /// connect refusal instead of a clean disconnect, which is not the scenario G6 means.
    /// The isolation is re-verified first: this command's whole target is
    /// `HERDR_SOCKET_PATH`.
    pub fn stop_server(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.isolation.assert_isolated()?;
        let mut cmd = std::process::Command::new(&self.bin);
        cmd.args(["server", "stop"]);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                cmd.env_remove(key);
            }
        }
        for (key, value) in self.isolation.env_pairs() {
            cmd.env(key, value);
        }
        for (key, value) in HERDR_OFFLINE_ENV {
            cmd.env(key, value);
        }
        cmd.env("SHELL", "/bin/sh");
        cmd.current_dir(&self.isolation.base);
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "`herdr server stop` exited {:?}: {}{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim(),
            )));
        }
        // The server process itself must go away too, or `respawn` would race it for the
        // socket path.
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    unregister_spawned_pid(self.pid);
                    self.pid = None;
                    return Ok(());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "herdr did not exit within {timeout:?} after `server stop`; ps says: {}",
                self.pid.map_or("(no pid)".to_string(), ps_snapshot)
            ),
        ))
    }

    /// Start a fresh server on the **same** socket path and config dir (scenario G6): the
    /// client's discovery is pinned to that path, so a restart anywhere else would not be a
    /// reconnect.
    ///
    /// `isolation` must be this spawner's own — it is taken as an argument so the call site
    /// reads as the kickoff names it, and is checked rather than trusted.
    pub fn respawn(&mut self, isolation: &HerdrIsolation) -> std::io::Result<()> {
        if isolation.socket_path != self.isolation.socket_path {
            return Err(std::io::Error::other(format!(
                "respawn: {} is not this server's socket ({})",
                isolation.socket_path.display(),
                self.isolation.socket_path.display()
            )));
        }
        if self.pid.is_some() {
            self.stop_server(Duration::from_secs(5))?;
        }
        let (master, child, pid, tail) = spawn_server_child(&self.bin, &self.isolation)?;
        self._master = master;
        self.child = child;
        self.pid = pid;
        self.tail = tail;
        Ok(())
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
                    "herdr exited before its socket appeared: {status:?}; it said: {}",
                    tail_text(&self.tail)
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
                "socket did not appear at {} within {timeout:?}; the server said: {}",
                self.isolation.socket_path.display(),
                tail_text(&self.tail)
            ),
        ))
    }

    /// Whether the PID matcher would accept this child (for tests of the matcher itself).
    pub fn matcher_accepts(&self) -> bool {
        self.pid
            .is_some_and(|pid| process_matches_binary(pid, &self.bin))
    }
}

/// Launch one `<bin> server` inside a PTY under `isolation`. Shared by
/// [`SpawnedHerdr::spawn`] and [`SpawnedHerdr::respawn`] so a restart cannot use a different
/// environment than the first start.
type SpawnedChild = (
    Box<dyn MasterPty + Send>,
    Box<dyn Child + Send + Sync>,
    Option<u32>,
    PtyTail,
);

/// The last [`PTY_TAIL_MAX`] bytes the server wrote to its pty, kept by the drain thread so
/// that a server which dies at startup can say why.
type PtyTail = Arc<Mutex<Vec<u8>>>;

/// How much of the server's own output to keep. Its startup banner and any panic fit easily.
const PTY_TAIL_MAX: usize = 8192;

/// The kept tail as one line, for an error message. Empty when the server said nothing.
fn tail_text(tail: &PtyTail) -> String {
    let bytes = tail.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let text = String::from_utf8_lossy(&bytes);
    let joined = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if joined.is_empty() {
        "(the server wrote nothing)".to_owned()
    } else {
        joined
    }
}

fn spawn_server_child(bin: &Path, isolation: &HerdrIsolation) -> std::io::Result<SpawnedChild> {
    isolation.assert_isolated()?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(std::io::Error::other)?;

    let mut cmd = CommandBuilder::new(bin);
    cmd.arg("server");
    // Clear every inherited HERDR_* first: our own shell may be inside herdr.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            cmd.env_remove(key);
        }
    }
    for (key, value) in isolation.env_pairs() {
        cmd.env(key, value);
    }
    for (key, value) in HERDR_OFFLINE_ENV {
        cmd.env(key, value);
    }
    cmd.env("SHELL", "/bin/sh");
    cmd.cwd(&isolation.base);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(std::io::Error::other)?;
    let pid = child.process_id();
    register_spawned_pid(pid, bin);

    // Drain the master, or the server cannot exit.
    //
    // `portable_pty` gives the child its own session (`setsid`) with this pty as its
    // *controlling* terminal (`unix.rs:255-274`). When such a process exits, the kernel
    // revokes the controlling terminal, and that blocks until the tty's output queue drains.
    // Nobody was reading this master, so a full queue left `herdr server stop` with a process
    // wedged in macOS `ps` state `E` ("trying to exit") forever — the G6 restart timed out.
    // Reading to EOF also ends the thread by itself when the master is dropped on respawn.
    // The last few KiB are kept so that a server which exits at startup can be quoted back
    // in the error instead of dying silently.
    let tail: PtyTail = Arc::new(Mutex::new(Vec::new()));
    if let Ok(mut reader) = pair.master.try_clone_reader() {
        let tail = Arc::clone(&tail);
        std::thread::spawn(move || {
            let mut sink = [0u8; 8192];
            while let Ok(n) = std::io::Read::read(&mut reader, &mut sink) {
                if n == 0 {
                    break;
                }
                let mut kept = tail.lock().unwrap_or_else(|e| e.into_inner());
                kept.extend_from_slice(&sink[..n]);
                if kept.len() > PTY_TAIL_MAX {
                    let cut = kept.len() - PTY_TAIL_MAX;
                    kept.drain(..cut);
                }
            }
        });
    }

    Ok((pair.master, child, pid, tail))
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

/// `ps -o pid=,ppid=,stat=,comm= -p <pid>`, for diagnostics in failure messages: a harness
/// that only says "it did not exit" cannot tell a live server from a zombie.
pub fn ps_snapshot(pid: u32) -> String {
    match std::process::Command::new("ps")
        .args(["-o", "pid=,ppid=,stat=,comm=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        Ok(_) => format!("(pid {pid} not in ps)"),
        Err(e) => format!("(ps failed: {e})"),
    }
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

    /// Two servers in one test binary must never share a socket path. macOS's clock is
    /// microsecond-grained, so the timestamp alone is not unique across threads that start
    /// together — and a shared path makes the second herdr exit with "already running".
    #[test]
    fn herdr_spawn_isolation_bases_are_unique_across_threads() {
        let dirs: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16).map(|_| scope.spawn(unique_test_dir)).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let unique: std::collections::BTreeSet<&PathBuf> = dirs.iter().collect();
        assert_eq!(
            unique.len(),
            dirs.len(),
            "colliding isolation bases: {dirs:?}"
        );
    }

    #[test]
    fn herdr_spawn_isolation_dirs_are_short_and_carry_the_offline_config() {
        let iso = HerdrIsolation::create().unwrap();
        assert!(iso.socket_path.as_os_str().len() < 100);
        assert!(iso.socket_path.to_string_lossy().starts_with("/tmp/lc-"));
        let config = std::fs::read_to_string(iso.config_home.join("herdr/config.toml")).unwrap();
        assert_eq!(config, HERDR_TEST_CONFIG);
        // The lines by name: the wizard off, and the two background fetches off under the
        // `[update]` header they belong to. A spawn missing either check line curls
        // herdr.dev, which is the network this suite promises never to touch, and a spawn
        // that writes them outside `[update]` is ignored just as quietly.
        for line in [
            "onboarding = false",
            "[update]",
            "version_check = false",
            "manifest_check = false",
        ] {
            assert!(
                config.lines().any(|written| written == line),
                "the spawned herdr config is missing `{line}`: {config:?}"
            );
        }
        let header = config.find("[update]").expect("the [update] header");
        for key in ["version_check", "manifest_check"] {
            assert!(
                config.find(key).expect("the key") > header,
                "`{key}` must sit under `[update]`, not above it: {config:?}"
            );
        }
        assert!(iso.home.is_dir());
        std::fs::remove_dir_all(&iso.base).unwrap();
    }

    #[test]
    fn herdr_spawn_offline_env_points_the_manifest_catalogue_at_a_dead_port() {
        // The belt beside the config's braces: whatever herdr does with `manifest_check`,
        // the catalogue URL it would fetch is a closed loopback port, not herdr.dev.
        assert_eq!(
            HERDR_OFFLINE_ENV,
            [(
                "HERDR_AGENT_DETECTION_MANIFEST_CATALOG_URL",
                "http://127.0.0.1:1/"
            )]
        );
        for (_, value) in HERDR_OFFLINE_ENV {
            assert!(
                value.starts_with("http://127.0.0.1:"),
                "the offline env must stay on loopback: {value}"
            );
        }
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
    fn herdr_spawn_isolation_env_covers_every_path_herdr_reads() {
        let iso = HerdrIsolation::create().unwrap();
        let pairs = iso.env_pairs();
        let names: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            vec![
                "XDG_CONFIG_HOME",
                "XDG_RUNTIME_DIR",
                "HOME",
                "XDG_STATE_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "HERDR_SOCKET_PATH",
            ]
        );
        // Every one of them stays inside the private base: this is what `assert_isolated`
        // checks before a spawn, a respawn or a `herdr server stop`.
        for (_, path) in &pairs {
            assert!(path.starts_with(&iso.base), "{}", path.display());
        }
        iso.assert_isolated().expect("a fresh isolation is private");
        std::fs::remove_dir_all(&iso.base).unwrap();
    }

    #[test]
    fn herdr_spawn_assert_isolated_refuses_a_real_looking_socket() {
        // The failure this guards: a hand-built isolation whose socket is the sponsor's own.
        let mut iso = HerdrIsolation::create().unwrap();
        let base = iso.base.clone();
        iso.socket_path = PathBuf::from("/Users/someone/.local/state/herdr/herdr.sock");
        let err = iso.assert_isolated().expect_err("escaping socket refused");
        assert!(err.to_string().contains("HERDR_SOCKET_PATH"), "{err}");
        // And a base that is not a private /tmp/lc-… dir at all.
        let outside = HerdrIsolation {
            base: PathBuf::from("/tmp/not-ours"),
            ..iso
        };
        let err = outside.assert_isolated().expect_err("foreign base refused");
        assert!(err.to_string().contains("refusing to touch it"), "{err}");
        std::fs::remove_dir_all(&base).unwrap();
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

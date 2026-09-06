//! The TUI PTY harness (Phase 3 kickoff deliverable 10): spawn a binary inside a real
//! pseudo-terminal of a fixed size, feed everything it writes to a `vt100` screen *and* to a
//! raw byte transcript, and poll the screen for what a test expects.
//!
//! Two views of the same output, on purpose: the [`vt100::Screen`] answers "what is on the
//! screen now" (text, per-cell attributes such as inversion, the alternate-screen and mouse
//! modes), while the raw transcript answers "which bytes were written" — the parser consumes
//! escape sequences, so the mouse-off / alternate-screen-exit assertions read the raw log.
//!
//! Imitates [`crate::herdr_spawn`]: `portable-pty` spawn, poll-for-ready (never
//! sleep-and-hope), the shared PID registry with kill-on-drop and kill-on-panic, safe
//! wrappers only (`unsafe_code = "forbid"`, no `libc`). The reader thread owns a clone of
//! the master; it ends when the child exits and the slave side closes.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

pub use vt100;

use crate::herdr_spawn::{register_spawned_pid, unregister_spawned_pid};

/// The default terminal size: `(cols, rows)`, the snapshot suite's 100×30.
pub const SIZE: (u16, u16) = (100, 30);
/// How often [`PtyTui::wait_for`] and [`PtyTui::wait_exit`] look again.
pub const POLL: Duration = Duration::from_millis(10);

/// What the reader thread fills: the parsed screen and the raw bytes, in order.
struct Shared {
    parser: vt100::Parser,
    raw: Vec<u8>,
    eof: bool,
}

/// A command to run inside a PTY; [`PtyCommand::spawn`] starts it.
#[derive(Debug, Clone)]
pub struct PtyCommand {
    bin: PathBuf,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    env_remove: Vec<OsString>,
    size: (u16, u16),
    sample_rss: bool,
}

impl PtyCommand {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            size: SIZE,
            sample_rss: false,
        }
    }

    /// Sample the child's resident set size for the life of the child (the Phase 4
    /// bench's `peak_rss_kb`): a sampler thread runs `ps -o rss= -p <pid>` — KiB on macOS
    /// and Linux alike — sleeps [`POLL`], and repeats, keeping the maximum, read with
    /// [`PtyTui::peak_rss_kb`]. The period is therefore ≈10 ms plus one `ps` (a few ms),
    /// not a strict tick. Off by default: that is a subprocess every ≈12 ms, which the
    /// timing scenes must not pay. Not `getrusage`: the testkit's `nix`
    /// has no `resource` feature, `ru_maxrss` differs in unit between the two OSes, and
    /// `RUSAGE_CHILDREN` reports the largest of every waited-for descendant, git included.
    pub fn sample_rss(mut self) -> Self {
        self.sample_rss = true;
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, args: I) -> Self {
        for a in args {
            self.args.push(a.as_ref().to_owned());
        }
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Set one variable for the child. Last call wins: setting a key that an earlier
    /// [`env_remove`](Self::env_remove) (or an earlier `env`) named replaces it, so a scene
    /// can override what [`isolated_lastcall`](Self::isolated_lastcall) chose.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_owned();
        self.env_remove.retain(|k| k != &key);
        self.env.retain(|(k, _)| k != &key);
        self.env.push((key, value.as_ref().to_owned()));
        self
    }

    /// Unset one variable for the child. Last call wins, the same way [`env`](Self::env)
    /// does: this drops any value an earlier `env` set for the key.
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_owned();
        self.env.retain(|(k, _)| k != &key);
        self.env_remove.push(key);
        self
    }

    /// `(cols, rows)`; the default is [`SIZE`].
    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.size = (cols, rows);
        self
    }

    /// The isolation every `lastcall` child gets in the e2e tier, exactly as the
    /// `status --json` golden spawns the binary: a private `HOME`, `LASTCALL_CONFIG`,
    /// `LASTCALL_STATE_DIR`, null global/system git config, no inherited XDG dirs, no
    /// inherited `LASTCALL_LOG*` (the transcript must carry no tracing) and no inherited
    /// `HERDR_*`; `TERM=xterm-256color` so crossterm sees a capable terminal.
    ///
    /// `LASTCALL_KEYBOARD=plain` skips the keyboard-enhancement probe (ruling P9): the
    /// harness answers no terminal query, so an unskipped probe would cost every scene
    /// crossterm's full 2 s timeout. A scene that wants the probe removes the variable.
    ///
    /// `$VISUAL` and `$EDITOR` are removed for the same kind of reason (Phase 8
    /// deliverable 7): they name a program `shift-i` would *run*.
    pub fn isolated_lastcall(self, home: &Path, config: &Path, state_dir: &Path) -> Self {
        let mut cmd = self
            .env("HOME", home)
            .env("LASTCALL_KEYBOARD", "plain")
            .env("LASTCALL_CONFIG", config)
            .env("LASTCALL_STATE_DIR", state_dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TERM", "xterm-256color")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("LASTCALL_LOG_FILE")
            .env_remove("LASTCALL_LOG")
            // Phase 8 deliverable 7: `shift-i` spawns whatever `$VISUAL`/`$EDITOR` names.
            // No scene may reach the developer's own editor — it would take the terminal
            // this harness is driving and wait for a human — so the isolation removes both
            // and the editor scenes set `EDITOR` to an absolute path inside their own temp
            // dir. Removing them here rather than in each scene means a scene that never
            // thought about editors cannot open one either.
            .env_remove("VISUAL")
            .env_remove("EDITOR");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                cmd = cmd.env_remove(key);
            }
        }
        cmd
    }

    /// Open the PTY and start the child. `ErrorKind::Unsupported` means this host cannot
    /// open a PTY at all (the only reason a PTY test may skip); anything else is a failure.
    pub fn spawn(self) -> io::Result<PtyTui> {
        let (cols, rows) = self.size;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e.to_string()))?;

        let mut cmd = CommandBuilder::new(&self.bin);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.cwd(dir);
        }
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        let child = pair.slave.spawn_command(cmd).map_err(io::Error::other)?;
        // The slave is dropped here (end of scope through `pair.slave`): the master reads
        // EOF once the child and its descendants have closed their copies.
        let pid = child.process_id();
        register_spawned_pid(pid, &self.bin);

        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let shared = Arc::new(Mutex::new(Shared {
            parser: vt100::Parser::new(rows, cols, 0),
            raw: Vec::new(),
            eof: false,
        }));
        let feed = shared.clone();
        std::thread::Builder::new()
            .name("lastcall-pty-reader".to_owned())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            lock(&feed).eof = true;
                            return;
                        }
                        Ok(n) => {
                            let mut s = lock(&feed);
                            s.raw.extend_from_slice(&buf[..n]);
                            s.parser.process(&buf[..n]);
                        }
                    }
                }
            })?;

        let rss = Arc::new(RssSampler::new());
        if let (true, Some(pid)) = (self.sample_rss, pid) {
            let sampler = rss.clone();
            let feed = shared.clone();
            std::thread::Builder::new()
                .name("lastcall-pty-rss".to_owned())
                .spawn(move || {
                    sample_loop(&sampler, || rss_kb_of(pid), || lock(&feed).eof);
                })?;
        }

        Ok(PtyTui {
            master: pair.master,
            writer,
            child,
            pid,
            shared,
            rss,
            bin: self.bin,
        })
    }
}

/// The peak resident set size seen by the sampler thread (`0` until the first sample),
/// and how the sampling went.
struct RssSampler {
    peak_kb: AtomicU64,
    /// Set by the harness once the child's exit was observed (`try_wait`) or on drop:
    /// the loop's clean end.
    stop: AtomicBool,
    /// Ticks at which `ps` gave no number while nothing said the child had exited.
    failed_ticks: AtomicU64,
    /// The loop gave up ([`RSS_GIVE_UP`] failed ticks in a row) before the child exited:
    /// the peak is partial.
    stopped_early: AtomicBool,
}

impl RssSampler {
    fn new() -> Self {
        Self {
            peak_kb: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            failed_ticks: AtomicU64::new(0),
            stopped_early: AtomicBool::new(false),
        }
    }
}

/// How many failed ticks in a row make [`sample_loop`] give up: about a second of
/// `ps` giving nothing for a child nobody has seen exit (`ps` missing altogether, say).
const RSS_GIVE_UP: u32 = 100;

/// The sampler thread's loop over any `sample` (the real one is `rss_kb_of(pid)`) until
/// `stop` is set or `exited()` holds (the reader's EOF: the child closed the slave).
/// A tick that yields no number is counted and skipped, never a reason to stop on its own:
/// `ps` can fail transiently (fork pressure under a 50,000-file drop), and a child that
/// has exited but is not yet reaped is a zombie `ps` reports as `0`, not an error.
/// Only [`RSS_GIVE_UP`] failures in a row end the loop early, recorded as `stopped_early`
/// so a bench can say its peak is partial rather than print a silently frozen number.
fn sample_loop(
    sampler: &RssSampler,
    mut sample: impl FnMut() -> Option<u64>,
    exited: impl Fn() -> bool,
) {
    let mut failures_in_a_row = 0u32;
    while !sampler.stop.load(Ordering::Relaxed) && !exited() {
        match sample() {
            Some(kb) => {
                sampler.peak_kb.fetch_max(kb, Ordering::Relaxed);
                failures_in_a_row = 0;
            }
            None => {
                if sampler.stop.load(Ordering::Relaxed) || exited() {
                    return; // reaped between the check and the sample: not a failure
                }
                sampler.failed_ticks.fetch_add(1, Ordering::Relaxed);
                failures_in_a_row += 1;
                if failures_in_a_row >= RSS_GIVE_UP {
                    sampler.stopped_early.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// `ps -o rss= -p <pid>`: the process's resident set in KiB; `None` once the process is
/// gone (or `ps` cannot be run).
fn rss_kb_of(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_rss(&String::from_utf8_lossy(&out.stdout))
}

/// The number in `ps -o rss=` output (whitespace around it, one line).
fn parse_rss(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// A child running inside a PTY. Killed on drop.
pub struct PtyTui {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    shared: Arc<Mutex<Shared>>,
    rss: Arc<RssSampler>,
    bin: PathBuf,
}

impl std::fmt::Debug for PtyTui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyTui")
            .field("pid", &self.pid)
            .field("bin", &self.bin)
            .finish()
    }
}

impl PtyTui {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The largest resident set size (KiB) the sampler saw so far; `None` when the
    /// command was not spawned with [`PtyCommand::sample_rss`] or no sample landed yet.
    pub fn peak_rss_kb(&self) -> Option<u64> {
        match self.rss.peak_kb.load(Ordering::Relaxed) {
            0 => None,
            kb => Some(kb),
        }
    }

    /// True when the sampler gave up before the child's exit was seen ([`RSS_GIVE_UP`]
    /// failed `ps` ticks in a row): [`peak_rss_kb`](Self::peak_rss_kb) covers only part
    /// of the child's life, and a bench must say so.
    pub fn rss_sampler_stopped_early(&self) -> bool {
        self.rss.stopped_early.load(Ordering::Relaxed)
    }

    /// How many ticks `ps` gave no number for the child while it was still running (each
    /// skipped, none fatal on its own).
    pub fn rss_failed_ticks(&self) -> u64 {
        self.rss.failed_ticks.load(Ordering::Relaxed)
    }

    /// The child's exit was observed: the sampler's clean end.
    fn exit_seen(&self) {
        self.rss.stop.store(true, Ordering::Relaxed);
    }

    /// Look at the parsed screen.
    pub fn screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        f(lock(&self.shared).parser.screen())
    }

    /// The screen as plain text, rows joined by `\n` (trailing blanks trimmed by vt100).
    pub fn screen_text(&self) -> String {
        self.screen(|s| s.contents())
    }

    /// The screen row by row, every column.
    pub fn rows(&self) -> Vec<String> {
        self.screen(|s| {
            let (_, cols) = s.size();
            s.rows(0, cols).collect()
        })
    }

    /// The first row (0-based) whose text satisfies `pred`.
    pub fn find_row(&self, mut pred: impl FnMut(&str) -> bool) -> Option<u16> {
        self.rows().iter().position(|r| pred(r)).map(|i| i as u16)
    }

    /// Whether the cell at `(row, col)` (0-based) is drawn inverted.
    pub fn inverse_at(&self, row: u16, col: u16) -> bool {
        self.screen(|s| s.cell(row, col).is_some_and(vt100::Cell::inverse))
    }

    /// Every byte the child wrote so far, escape sequences included.
    pub fn raw(&self) -> Vec<u8> {
        lock(&self.shared).raw.clone()
    }

    /// True once the reader saw EOF (the child and its descendants closed the slave).
    pub fn eof(&self) -> bool {
        lock(&self.shared).eof
    }

    /// Poll the screen every [`POLL`] until `pred` holds; returns how long that took.
    /// Fails at once (with the screen) if the child exits first, and at `timeout`.
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&vt100::Screen) -> bool,
    ) -> Result<Duration, String> {
        let start = Instant::now();
        loop {
            if self.screen(&mut pred) {
                return Ok(start.elapsed());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit_seen();
                // One last look: the final bytes may land after the exit is observed.
                std::thread::sleep(POLL);
                if self.screen(&mut pred) {
                    return Ok(start.elapsed());
                }
                return Err(format!(
                    "child exited ({status:?}) before the screen matched; screen:\n{}",
                    self.screen_text()
                ));
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "screen did not match within {timeout:?}; screen:\n{}",
                    self.screen_text()
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    /// [`wait_for`](Self::wait_for) on "some row contains `needle`".
    pub fn wait_for_text(&mut self, needle: &str, timeout: Duration) -> Result<Duration, String> {
        let needle = needle.to_owned();
        self.wait_for(timeout, |s| s.contents().contains(&needle))
    }

    /// Wait for the reader to see EOF (after an exit, so the transcript is complete).
    pub fn wait_eof(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while !self.eof() {
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(POLL);
        }
        true
    }

    /// Write bytes to the child's terminal (keys, mouse reports).
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// A left click (SGR press then release) at `(col, row)`, 0-based screen coordinates.
    pub fn click(&mut self, col: u16, row: u16) -> io::Result<()> {
        self.send(&sgr_click(col, row))
    }

    /// Change the terminal size (`(cols, rows)`); the child receives `SIGWINCH` and the
    /// parsed screen is resized to match.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;
        lock(&self.shared).parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Poll for the child's exit every [`POLL`], up to `timeout`.
    pub fn wait_exit(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.exit_seen();
                return Ok(status);
            }
            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("child still running after {timeout:?}"),
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for PtyTui {
    fn drop(&mut self) {
        self.rss.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        unregister_spawned_pid(self.pid);
    }
}

/// The SGR (`?1006`) left-button press and release for `(col, row)`, 0-based: what a
/// terminal sends for a click once crossterm has enabled mouse capture.
pub fn sgr_click(col: u16, row: u16) -> Vec<u8> {
    format!(
        "\x1b[<0;{};{}M\x1b[<0;{};{}m",
        col + 1,
        row + 1,
        col + 1,
        row + 1
    )
    .into_bytes()
}

/// The column (0-based, in cells) at which `needle` starts in a screen row, counting
/// characters rather than bytes — right for rows of single-width characters.
pub fn col_of(row: &str, needle: &str) -> Option<u16> {
    let byte = row.find(needle)?;
    Some(row[..byte].chars().count() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SH: &str = "/bin/sh";

    fn skip_if_no_pty(err: &io::Error) -> bool {
        if err.kind() == io::ErrorKind::Unsupported {
            let mut e = io::stderr();
            let _ = e.write_all(format!("SKIP: cannot open a pty here: {err}\n").as_bytes());
            true
        } else {
            false
        }
    }

    #[test]
    fn pty_tui_sgr_click_is_one_based_press_then_release() {
        assert_eq!(sgr_click(0, 0), b"\x1b[<0;1;1M\x1b[<0;1;1m");
        assert_eq!(sgr_click(2, 7), b"\x1b[<0;3;8M\x1b[<0;3;8m");
        assert_eq!(col_of("│beta  main", "main"), Some(7));
        assert_eq!(col_of("  M f1  +1 −1", "−1"), Some(11));
        assert_eq!(col_of("abc", "zzz"), None);
    }

    #[test]
    fn pty_tui_parses_the_screen_the_attributes_the_raw_bytes_and_the_exit_code() {
        let spawned = PtyCommand::new(SH)
            .arg("-c")
            .arg("printf 'hello \\033[7mINV\\033[0m'; exit 3")
            .size(40, 5)
            .spawn();
        let mut pty = match spawned {
            Ok(p) => p,
            Err(e) if skip_if_no_pty(&e) => return,
            Err(e) => panic!("spawn: {e}"),
        };
        pty.wait_for_text("INV", Duration::from_secs(5))
            .expect("the shell's output reaches the screen");
        assert_eq!(pty.rows()[0].trim_end(), "hello INV");
        assert!(!pty.inverse_at(0, 0), "plain text");
        assert!(pty.inverse_at(0, 6), "INV is inverted");
        assert_eq!(pty.find_row(|r| r.contains("INV")), Some(0));
        let status = pty.wait_exit(Duration::from_secs(5)).expect("exits");
        assert_eq!(status.exit_code(), 3);
        assert!(pty.wait_eof(Duration::from_secs(2)));
        let raw = pty.raw();
        assert!(
            raw.windows(4).any(|w| w == b"\x1b[7m"),
            "the raw transcript keeps the escape sequence: {raw:?}"
        );
        assert!(pty.screen(|s| !s.alternate_screen()));
    }

    #[test]
    fn pty_tui_parse_rss_reads_the_ps_column() {
        assert_eq!(parse_rss("  1234\n"), Some(1234));
        assert_eq!(parse_rss("98765"), Some(98765));
        assert_eq!(parse_rss(""), None, "no such process: empty output");
        assert_eq!(parse_rss("  RSS\n 12\n"), None, "a header is not a sample");
        assert_eq!(rss_kb_of(0), None, "pid 0 is never one of ours");
    }

    /// A failed `ps` tick is skipped, not the end of sampling: the peak keeps moving
    /// after it, the failure is counted, and only a run of [`RSS_GIVE_UP`] failures
    /// stops the loop early (and says so).
    #[test]
    fn pty_tui_rss_sampler_skips_a_failed_tick_and_keeps_going() {
        assert_eq!(rss_kb_of(u32::MAX), None, "a bogus pid is a failed tick");

        let sampler = RssSampler::new();
        let ticks = [None, Some(100), None, None, Some(300), Some(200)];
        let mut i = 0;
        sample_loop(
            &sampler,
            || {
                let tick = ticks[i];
                i += 1;
                if i == ticks.len() {
                    sampler.stop.store(true, Ordering::Relaxed); // the harness saw the exit
                }
                tick
            },
            || false,
        );
        assert_eq!(i, ticks.len(), "every tick was taken");
        assert_eq!(
            sampler.peak_kb.load(Ordering::Relaxed),
            300,
            "the peak moved past the failed ticks"
        );
        assert_eq!(sampler.failed_ticks.load(Ordering::Relaxed), 3);
        assert!(!sampler.stopped_early.load(Ordering::Relaxed));

        let exited = RssSampler::new();
        sample_loop(&exited, || None, || true);
        assert_eq!(
            exited.failed_ticks.load(Ordering::Relaxed),
            0,
            "the child is gone (EOF): nothing to sample, nothing failed"
        );
        assert!(!exited.stopped_early.load(Ordering::Relaxed));

        let gave_up = RssSampler::new();
        let mut asked = 0u32;
        sample_loop(
            &gave_up,
            || {
                asked += 1;
                None
            },
            || false,
        );
        assert_eq!(
            asked, RSS_GIVE_UP,
            "gives up after RSS_GIVE_UP failures in a row"
        );
        assert!(gave_up.stopped_early.load(Ordering::Relaxed));
        assert_eq!(gave_up.peak_kb.load(Ordering::Relaxed), 0);
    }

    /// The real sampler over a live child that exits on its own: the peak is a live
    /// process's, the exit observed by `wait_exit` ends the loop cleanly (the zombie
    /// between exit and reap is a `0` sample, not a failure), nothing stopped early.
    #[test]
    fn pty_tui_rss_sampler_runs_to_the_childs_exit() {
        let spawned = PtyCommand::new(SH)
            .arg("-c")
            .arg("echo ready; sleep 0.3")
            .size(40, 5)
            .sample_rss()
            .spawn();
        let mut pty = match spawned {
            Ok(p) => p,
            Err(e) if skip_if_no_pty(&e) => return,
            Err(e) => panic!("spawn: {e}"),
        };
        pty.wait_for_text("ready", Duration::from_secs(5))
            .expect("the shell is up");
        let status = pty.wait_exit(Duration::from_secs(5)).expect("exits");
        assert_eq!(status.exit_code(), 0);
        assert!(pty.rss.stop.load(Ordering::Relaxed), "the exit was seen");
        let peak = pty.peak_rss_kb().expect("sampled while it lived");
        assert!(peak > 0, "{peak} KiB");
        assert!(!pty.rss_sampler_stopped_early());
        assert_eq!(pty.rss_failed_ticks(), 0, "no tick failed on a live child");
    }

    #[test]
    fn pty_tui_samples_the_childs_peak_rss_only_when_asked() {
        let spawned = PtyCommand::new(SH)
            .arg("-c")
            .arg("echo ready; while :; do sleep 0.02; done")
            .size(40, 5)
            .sample_rss()
            .spawn();
        let mut pty = match spawned {
            Ok(p) => p,
            Err(e) if skip_if_no_pty(&e) => return,
            Err(e) => panic!("spawn: {e}"),
        };
        pty.wait_for_text("ready", Duration::from_secs(5))
            .expect("the shell is up");
        let deadline = Instant::now() + Duration::from_secs(5);
        let peak = loop {
            if let Some(kb) = pty.peak_rss_kb() {
                break kb;
            }
            assert!(Instant::now() < deadline, "no RSS sample within 5 s");
            std::thread::sleep(POLL);
        };
        assert!(peak > 0, "a live shell has a resident set: {peak} KiB");
        assert!(
            peak < 1_000_000,
            "a shell is not a gigabyte: {peak} KiB (unit is KiB, not bytes)"
        );
        drop(pty);

        let silent = PtyCommand::new(SH)
            .arg("-c")
            .arg("echo ready; while :; do sleep 0.02; done")
            .size(40, 5)
            .spawn();
        let mut pty = match silent {
            Ok(p) => p,
            Err(e) if skip_if_no_pty(&e) => return,
            Err(e) => panic!("spawn: {e}"),
        };
        pty.wait_for_text("ready", Duration::from_secs(5))
            .expect("the shell is up");
        assert_eq!(pty.peak_rss_kb(), None, "not asked: nothing sampled");
    }

    #[test]
    fn pty_tui_resize_reaches_the_child_as_sigwinch() {
        let spawned = PtyCommand::new(SH)
            .arg("-c")
            .arg("trap 'stty size' WINCH; stty size; while :; do sleep 0.02; done")
            .size(100, 30)
            .spawn();
        let mut pty = match spawned {
            Ok(p) => p,
            Err(e) if skip_if_no_pty(&e) => return,
            Err(e) => panic!("spawn: {e}"),
        };
        pty.wait_for_text("30 100", Duration::from_secs(5))
            .expect("initial size printed");
        pty.resize(40, 12).expect("resize");
        assert_eq!(pty.screen(|s| s.size()), (12, 40));
        pty.wait_for_text("12 40", Duration::from_secs(5))
            .expect("the child saw SIGWINCH and the new size");
        // Dropping kills the loop; the registry forgets the pid.
        let pid = pty.pid().expect("pid");
        drop(pty);
        assert!(!crate::herdr_spawn::kill_registered(pid));
    }
}

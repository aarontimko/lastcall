//! Engine helpers for scenario tests: open an engine over a fixture, assert a pile in the
//! harness's exact format, and the SIGKILL fault injector for E1.

use std::path::{Path, PathBuf};

use lastcall_engine::config::{Config, ConfigSource, Loaded, Resolved};
use lastcall_engine::engine::{Engine, EngineOptions};
use lastcall_engine::env::Env;
use lastcall_engine::ops::{FaultInjector, FaultPoint};

pub use lastcall_engine::scan::pile_lines;

/// Open an engine whose only configured parent dir is `parent` (canonicalized), with the
/// given config (its `parent_dirs` is overwritten) and state dir.
pub fn open_engine(parent: &Path, env: &Env, state_dir: &Path, config: Config) -> Engine {
    open_engine_with(parent, env, state_dir, config, EngineOptions::default())
}

/// [`open_engine`] with explicit options (compaction threshold, clock).
pub fn open_engine_with(
    parent: &Path,
    env: &Env,
    state_dir: &Path,
    config: Config,
    options: EngineOptions,
) -> Engine {
    let (loaded, resolved) = loaded_for(parent, state_dir, config);
    Engine::open(&loaded, &resolved, env, options).expect("engine opens")
}

/// The `Loaded`/`Resolved` pair a config file with `parent_dirs = [parent]` would give.
pub fn loaded_for(parent: &Path, state_dir: &Path, config: Config) -> (Loaded, Resolved) {
    let parent: PathBuf = std::fs::canonicalize(parent).expect("parent exists");
    let loaded = Loaded {
        config: Config {
            parent_dirs: vec![parent.clone()],
            ..config
        },
        source: ConfigSource::Defaults { searched: vec![] },
        state_dir: state_dir.to_path_buf(),
    };
    let resolved = Resolved {
        parent_dirs: vec![parent],
        notices: vec![],
    };
    (loaded, resolved)
}

/// The harness's `lc_pile | tr '\n' '|'` string for a pile.
pub fn pile_string(pile: &lastcall_engine::scan::Pile) -> String {
    pile_lines(pile).join("|")
}

/// `assert_pile!(engine, root, "f2 mixed|u1 upstream")`: scan `root` now and compare
/// against the harness's expected string (sorted lines joined by `|`, `""` = empty).
#[macro_export]
macro_rules! assert_pile {
    ($engine:expr, $root:expr, $expected:expr) => {{
        let pile = $engine.scan(&$root).expect("scan succeeds");
        let got = $crate::engine::pile_string(&pile);
        assert_eq!(
            got, $expected,
            "pile mismatch (got [{got}], expected [{}])",
            $expected
        );
        pile
    }};
    ($engine:expr, $root:expr, $expected:expr, $label:expr) => {{
        let pile = $engine.scan(&$root).expect("scan succeeds");
        let got = $crate::engine::pile_string(&pile);
        assert_eq!(
            got, $expected,
            "{}: pile mismatch (got [{got}], expected [{}])",
            $label, $expected
        );
        pile
    }};
}

/// SIGKILLs the current process when the given fault point is reached (E1). Test-only:
/// the engine's production code passes `NoFault`.
#[derive(Debug, Clone, Copy)]
pub struct KillAt(pub FaultPoint);

impl FaultInjector for KillAt {
    fn at(&self, point: FaultPoint) {
        if point == self.0 {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::SIGKILL);
            // A self-directed SIGKILL from a non-main thread is delivered asynchronously
            // on macOS: this thread keeps running (and would finish the rename) until
            // the kernel gets around to tearing the process down. Never return.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::process::abort();
        }
    }
}

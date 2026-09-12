use std::time::Duration;

use lastcall_engine::engine::EngineOptions;
use lastcall_engine::watcher::EngineTimings;

/// The engine knobs the binary sets. Everything is a default except the pool width, which
/// `LASTCALL_PARALLELISM` may pin: there is no config key for it (Phase 5 ruling), and the
/// override exists so the golden test can run the same command at width 1 and width 8 and
/// diff the output. An unparsable or zero value is ignored, not an error.
pub fn engine_options() -> EngineOptions {
    let mut options = EngineOptions::default();
    let raw = std::env::var_os("LASTCALL_PARALLELISM");
    if let Some(n) = parallelism_override(raw.as_ref().and_then(|v| v.to_str())) {
        options.parallelism = n;
    }
    options
}

/// The parsing half of [`engine_options`], separated so it is testable without touching the
/// process environment. Anything that is not a positive integer is ignored.
fn parallelism_override(raw: Option<&str>) -> Option<usize> {
    raw?.trim().parse::<usize>().ok().filter(|n| *n >= 1)
}

pub mod config;
pub mod hello_herdr;
pub mod status;
pub mod tui;
pub mod update;
pub mod watch;

/// The `--poll <secs>` backstop shared by `watch` and `tui`: `None` keeps the engine's
/// defaults (HEAD every 10 s, rescan every 30 s); `Some(n)` sets both to `n` seconds,
/// clamped to at least 1 s.
pub fn poll_timings(poll: Option<u64>) -> EngineTimings {
    let mut timings = EngineTimings::default();
    if let Some(secs) = poll {
        let every = Duration::from_secs(secs.max(1));
        timings.head_poll = every;
        timings.rescan = every;
    }
    timings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_timings_none_keeps_defaults_and_zero_clamps_to_one_second() {
        let default = EngineTimings::default();
        let none = poll_timings(None);
        assert_eq!(none.head_poll, default.head_poll);
        assert_eq!(none.rescan, default.rescan);
        let one = poll_timings(Some(0));
        assert_eq!(one.head_poll, Duration::from_secs(1));
        assert_eq!(one.rescan, Duration::from_secs(1));
        // `--poll` moves the two backstops and nothing else: the debounce and its 3 s
        // starvation cap (Amendment v1.6) are hardcoded, so they survive it unchanged.
        assert_eq!(one.debounce, default.debounce);
        assert_eq!(one.debounce_max, default.debounce_max);
        let five = poll_timings(Some(5));
        assert_eq!(five.head_poll, Duration::from_secs(5));
        assert_eq!(five.rescan, Duration::from_secs(5));
    }

    #[test]
    fn parallelism_override_takes_a_positive_integer_and_ignores_the_rest() {
        assert_eq!(parallelism_override(Some("1")), Some(1));
        assert_eq!(parallelism_override(Some(" 8\n")), Some(8));
        // Above the engine's ceiling is not an error here; the engine clamps.
        assert_eq!(parallelism_override(Some("99")), Some(99));
        assert_eq!(parallelism_override(Some("0")), None);
        assert_eq!(parallelism_override(Some("-2")), None);
        assert_eq!(parallelism_override(Some("many")), None);
        assert_eq!(parallelism_override(Some("")), None);
        assert_eq!(parallelism_override(None), None);
        // The default is never zero, whatever the machine reports.
        assert!(EngineOptions::default().parallelism >= 1);
    }
}

use std::time::Duration;

use lastcall_engine::watcher::EngineTimings;

pub mod config;
pub mod hello_herdr;
pub mod status;
pub mod tui;
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
        assert_eq!(one.debounce, default.debounce);
        let five = poll_timings(Some(5));
        assert_eq!(five.head_poll, Duration::from_secs(5));
        assert_eq!(five.rescan, Duration::from_secs(5));
    }
}

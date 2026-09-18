//! Golden: `lastcall status --json` (the built binary) over the three-root parent dir from
//! `lastcall_testkit::fixture_parent`, byte-for-byte against
//! `tests/golden/status_multi_repo.json` with the parent dir replaced by `<W>`.
//!
//! `LASTCALL_UPDATE_GOLDEN=1` (`just golden-update`) rewrites the file instead of comparing.
//!
//! The report is produced twice — once with the root pool pinned to one thread and once
//! with it pinned to eight (`LASTCALL_PARALLELISM`, the binary-only override) — and both
//! must equal the same golden file. That is the end-to-end form of Phase 5's rule that the
//! bounded pool is a performance change and nothing else.

use std::path::Path;
use std::process::Command;

use lastcall_testkit::fixture_parent;
use lastcall_testkit::tmp::TempDir;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/status_multi_repo.json"
);

/// One `lastcall status --json` run over a freshly built fixture parent, with the root
/// pool pinned to `parallelism` threads. The state dir is fresh each time, so both runs do
/// their own first sight.
fn status_json(parallelism: usize) -> String {
    let w = TempDir::new("lc-golden-w");
    let state = TempDir::new("lc-golden-state");
    let built =
        fixture_parent::build(w.path(), state.path(), &state.join("home")).expect("fixture builds");
    let config = state.join("config.toml");
    fixture_parent::write_config(&config, w.path()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_lastcall"))
        .args(["status", "--json"])
        .current_dir(w.path())
        .env("HOME", &built.home)
        .env("LASTCALL_CONFIG", &config)
        .env("LASTCALL_STATE_DIR", state.path())
        .env("LASTCALL_PARALLELISM", parallelism.to_string())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .expect("run lastcall");
    assert!(
        out.status.success(),
        "status --json at parallelism {parallelism} exit {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let canonical = std::fs::canonicalize(w.path()).unwrap();
    let actual = String::from_utf8(out.stdout)
        .expect("utf-8 json")
        .replace(&canonical.to_string_lossy().to_string(), "<W>")
        .replace(&w.path().to_string_lossy().to_string(), "<W>");
    assert!(
        actual.contains("\"status_version\": 1"),
        "not a status report:\n{actual}"
    );

    // Amendment v1.9: the state dir is *named* now, so the rule is no longer "it never
    // appears" but "it appears only as `state_dir`" — a leak anywhere else is still a leak.
    let state_str = state.path().to_string_lossy().to_string();
    let carriers: Vec<&str> = actual.lines().filter(|l| l.contains(&state_str)).collect();
    assert!(
        carriers.len() == 1 && carriers[0].trim_start().starts_with("\"state_dir\": "),
        "the state dir appears outside `state_dir`:\n{carriers:?}"
    );

    // The three v1.9 fields are per-run values (a temp state dir, the hash of a temp root
    // path, a wall-clock mtime), so the golden pins their shape, not their content.
    let normalised: Vec<String> = actual
        .lines()
        .map(|line| {
            redact(line, "state_dir", "<S>")
                .or_else(|| redact(line, "store", "<H>"))
                .or_else(|| redact(line, "ledger_written_at", "<T>"))
                .unwrap_or_else(|| line.to_owned())
        })
        .collect();
    let mut out = normalised.join("\n");
    out.push('\n');
    for token in ["\"<S>\"", "\"<H>\"", "\"<T>\""] {
        assert!(out.contains(token), "{token} was never produced:\n{out}");
    }
    out
}

/// Replace a `"<key>": "<string>"` line's value with `token`, keeping the indent and any
/// trailing comma. `None` when the line is not that key, or when its value is not a
/// string — a `null` `ledger_written_at` stays `null` and is visible in the golden.
fn redact(line: &str, key: &str, token: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let head = format!("\"{key}\": ");
    let value = trimmed.strip_prefix(&head)?;
    let (value, comma) = match value.strip_suffix(',') {
        Some(v) => (v, ","),
        None => (value, ""),
    };
    value
        .starts_with('"')
        .then(|| format!("{indent}{head}\"{token}\"{comma}"))
}

#[test]
fn status_json_multi_repo_matches_golden() {
    let actual = status_json(1);
    let wide = status_json(8);
    assert!(
        wide == actual,
        "the eight-thread run differs from the one-thread run\n         --- parallelism 1 ---\n{actual}\n--- parallelism 8 ---\n{wide}"
    );

    if std::env::var_os("LASTCALL_UPDATE_GOLDEN").is_some() {
        std::fs::write(GOLDEN, &actual).expect("write golden");
        eprintln!("golden rewritten: {GOLDEN}");
        return;
    }
    let expected = std::fs::read_to_string(Path::new(GOLDEN))
        .unwrap_or_else(|e| panic!("read {GOLDEN}: {e} (run `just golden-update`)"));
    assert!(
        actual == expected,
        "status --json differs from {GOLDEN} (run `just golden-update` if intended)\n\
         --- expected ---\n{expected}\n--- actual ---\n{actual}"
    );
}

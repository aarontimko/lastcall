//! Golden: `lastcall status --json` (the built binary) over the three-root parent dir from
//! `lastcall_testkit::fixture_parent`, byte-for-byte against
//! `tests/golden/status_multi_repo.json` with the parent dir replaced by `<W>`.
//!
//! `LASTCALL_UPDATE_GOLDEN=1` (`just golden-update`) rewrites the file instead of comparing.

use std::path::Path;
use std::process::Command;

use lastcall_testkit::fixture_parent;
use lastcall_testkit::tmp::TempDir;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/status_multi_repo.json"
);

#[test]
fn status_json_multi_repo_matches_golden() {
    let w = TempDir::new("lc-golden-w");
    let state = TempDir::new("lc-golden-state");
    let built = fixture_parent::build(w.path(), state.path()).expect("fixture builds");
    let config = state.join("config.toml");
    fixture_parent::write_config(&config, w.path()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_lastcall"))
        .args(["status", "--json"])
        .current_dir(w.path())
        .env("HOME", &built.home)
        .env("LASTCALL_CONFIG", &config)
        .env("LASTCALL_STATE_DIR", state.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .expect("run lastcall");
    assert!(
        out.status.success(),
        "status --json exit {:?}\nstderr:\n{}",
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
    assert!(
        !actual.contains(&state.path().to_string_lossy().to_string()),
        "the state dir leaked into the report:\n{actual}"
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

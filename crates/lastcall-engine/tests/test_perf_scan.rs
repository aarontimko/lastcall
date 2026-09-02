//! Not a gate. Evidence for the scan's process budget when *everything* is unseen (a null
//! seen tree after an unreadable ledger, a moved-aside ledger, `draft_initial = pending`,
//! a big branch switch): every tracked file is a row, and rendering each row must not cost
//! a subprocess. Run with
//!
//! ```text
//! cargo test -p lastcall-engine --test test_perf_scan -- --ignored --nocapture
//! ```
//!
//! and read the `PERF` lines: git spawns (process-wide, both runners) and wall time per
//! scan, then the no-change scan over a seen tree (must hash nothing).

use std::time::Instant;

use lastcall_engine::config::Config;
use lastcall_engine::git::spawn_count;
use lastcall_engine::ops::NoFault;
use lastcall_testkit::engine::open_engine;
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::tmp::TempDir;

const FILES: usize = 2000;

#[test]
#[ignore]
fn perf_scan_everything_unseen_2000_files() {
    let mut repo = FixtureRepo::new("perf").unwrap();
    let files: Vec<(String, String)> = (0..FILES)
        .map(|i| {
            (
                format!("src/d{:02}/f{i:04}.txt", i % 50),
                format!("line one of {i}\nline two\nline three\n"),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    repo.commit_files(&refs, "2000 files").unwrap();
    let state = TempDir::new("lc-perf-state");
    let env = repo.engine_env(state.path());
    let engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    let root = engine.root_paths()[0].clone();
    let ledger = engine.root(&root).unwrap().paths.ledger.clone();
    drop(engine);

    // Everything unseen: the ledger goes unreadable; the reopen has a null seen tree.
    std::fs::write(&ledger, b"{ not json").unwrap();
    let mut engine = open_engine(repo.parent_dir(), &env, state.path(), Config::default());
    for pass in 0..2 {
        let before = spawn_count();
        let t = Instant::now();
        let pile = engine.scan(&root).unwrap();
        eprintln!(
            "PERF everything-unseen pass {pass}: rows={} git spawns={} wall={:?}",
            pile.rows.len(),
            spawn_count() - before,
            t.elapsed()
        );
        assert!(pile.rows.len() >= FILES, "{}", pile.rows.len());
    }

    // Accept all, then the no-change scan: no hash-object at all.
    let pile = engine.scan(&root).unwrap();
    let t = Instant::now();
    let before = spawn_count();
    engine
        .ops(&root)
        .unwrap()
        .accept_all(&pile, &NoFault)
        .unwrap();
    eprintln!(
        "PERF accept-all: git spawns={} wall={:?}",
        spawn_count() - before,
        t.elapsed()
    );
    let hashes_before = engine.root(&root).unwrap().store.git().hash_object_calls();
    let before = spawn_count();
    let t = Instant::now();
    let pile = engine.scan(&root).unwrap();
    let hashes = engine.root(&root).unwrap().store.git().hash_object_calls() - hashes_before;
    eprintln!(
        "PERF no-change: rows={} git spawns={} hash-object calls={hashes} wall={:?}",
        pile.rows.len(),
        spawn_count() - before,
        t.elapsed()
    );
    assert!(pile.rows.is_empty());
    assert_eq!(hashes, 0, "a no-change scan hashes nothing");
}

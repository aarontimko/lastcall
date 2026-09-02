//! Placeholder so `cargo test --workspace --test 'test_e2e_*'` has a match (cargo errors on a
//! `--test` glob with zero matches). Phase 3 (Ratatui `TestBackend` snapshots) and Phase 9
//! (PTY-driven binary tests) replace this file.

#[test]
fn e2e_placeholder_until_phase_3() {
    assert_eq!(2 + 2, 4);
}

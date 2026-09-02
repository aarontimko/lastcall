//! Determinism of the fixture-repo builder (kickoff deliverable 9): two builds with the same
//! script yield identical `HEAD` hashes — the identity, dates, and config are all pinned, so
//! Phase 2's git-plumbing tests can assert on exact object ids. Real git, no network.

use lastcall_testkit::fixture_repo::FixtureRepo;

fn script(name: &str) -> (String, String, String) {
    let mut repo = FixtureRepo::new(name).unwrap();
    let seed = repo.head().unwrap();
    let edit = repo
        .commit_files(
            &[("f1", "a1 CHANGED\n"), ("src/new.rs", "fn main() {}\n")],
            "edit",
        )
        .unwrap();
    let pushed = repo.coworker_push(3).unwrap();
    assert_ne!(seed, edit);
    assert_ne!(edit, pushed);
    (seed, edit, pushed)
}

#[test]
fn fixture_repo_builds_are_deterministic() {
    let first = script("det");
    let second = script("det");
    assert_eq!(first, second, "same script, same hashes");
    // The name is part of nothing that git hashes (paths are relative to the work tree).
    let third = script("other-name");
    assert_eq!(first, third);
}

#[test]
fn fixture_repo_seed_hash_is_stable_across_processes() {
    // Pinned so a change to the seed files, identity, dates, or message is a visible diff
    // (git object ids depend on exactly those, not on the machine, path, or git version).
    let repo = FixtureRepo::new("stable").unwrap();
    assert_eq!(
        repo.head().unwrap(),
        "2d06521f7566830cbcde5b3753f450dc35d6dbba",
        "seed commit hash moved: the fixture inputs changed"
    );
}

//! Kickoff deliverable 9(a): the engine-side accept loop end to end, driven through
//! [`Engine::accept`] (the TUI's path) against a fixture "agent" that edits and commits
//! behind the reviewer's back. Every step asserts the pile (harness string form) and the
//! ledger: the override map first, then the folded seen tree and `seen_at.head_commit`.
//!
//! The point of the loop is the seen-tree model (docs/spec/00-spec.md §6.2): the agent's
//! commits move HEAD but never a baseline, so a file the agent committed is still pending
//! until the reviewer accepts it, and a file the reviewer accepted stays quiet across the
//! agent's later commit of it.

use lastcall_engine::config::Config;
use lastcall_engine::engine::{AcceptRequest, Accepted, Engine};
use lastcall_engine::git::Oid;
use lastcall_engine::ops::Rendered;
use lastcall_engine::scan::Pile;
use lastcall_testkit::assert_pile;
use lastcall_testkit::engine::{open_engine, pile_string};
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::tmp::TempDir;

/// Seed `f1` with line 1 and line 10 changed: two separated hunks.
const F1_TWO_HUNKS: &str = "A1\na2\na3\na4\na5\na6\na7\na8\na9\nA10\n";
/// `F1_TWO_HUNKS` with only the first hunk applied to the seed.
const F1_HUNK_ONE_APPLIED: &str = "A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n";
const F2_EDIT: &str = "b\nb2\n";
const F3_EDIT: &str = "c changed\n";
const F2_EDIT_AGAIN: &str = "b\nb2\nb3\n";

/// The fixture agent: a process the reviewer does not control, editing the repo `alpha`
/// and committing when it feels like it.
struct Agent {
    repo: FixtureRepo,
}

impl Agent {
    fn new() -> Self {
        Self {
            repo: FixtureRepo::new("alpha").expect("fixture repo"),
        }
    }

    /// Edit `f1` (two hunks), `f2` and `f3`; commit `f3` only. Returns the commit.
    fn edit_three_commit_one(&self) -> String {
        self.repo.write("f1", F1_TWO_HUNKS);
        self.repo.write("f2", F2_EDIT);
        self.repo.write("f3", F3_EDIT);
        self.repo.git(&["add", "f3"]).expect("git add f3");
        self.repo
            .git(&["commit", "-q", "-m", "agent: f3"])
            .expect("git commit f3");
        self.repo.head().expect("HEAD")
    }

    /// `git commit -a`: the rest of the working tree (`f1`, `f2`).
    fn commit_the_rest(&self) -> String {
        self.repo
            .git(&["commit", "-q", "-a", "-m", "agent: f1 f2"])
            .expect("git commit -a");
        self.repo.head().expect("HEAD")
    }

    fn edit_one_again(&self) {
        self.repo.write("f2", F2_EDIT_AGAIN);
    }

    fn tree_of_head(&self) -> String {
        self.repo
            .git(&["rev-parse", "HEAD^{tree}"])
            .expect("rev-parse")
            .trim()
            .to_owned()
    }
}

/// The reviewer's side: one engine over `alpha`, its root path, and the accept helpers
/// the TUI would call.
struct Reviewer {
    engine: Engine,
    root: std::path::PathBuf,
}

impl Reviewer {
    fn open(agent: &Agent, state: &TempDir) -> Self {
        let env = agent.repo.engine_env(state.path());
        let engine = open_engine(agent.repo.path(), &env, state.path(), Config::default());
        let root = engine
            .roots()
            .iter()
            .find(|r| r.path == std::fs::canonicalize(agent.repo.path()).unwrap())
            .map(|r| r.path.clone())
            .expect("alpha is a root");
        Self { engine, root }
    }

    /// `Engine::accept` (the TUI path): Ok, not refused, and exactly one rescan whose
    /// seq the `Accepted` carries.
    fn accept(&mut self, req: AcceptRequest) -> Accepted {
        let before = self.engine.scan_seq();
        let accepted = self.engine.accept(&self.root, req).expect("accept is Ok");
        assert!(accepted.outcome.ok(), "refused: {:?}", accepted.outcome);
        assert_eq!(accepted.seq, before + 1, "one rescan per accept");
        assert_eq!(accepted.seq, self.engine.scan_seq());
        accepted
    }

    fn rendered(&mut self, path: &str) -> (Rendered, lastcall_engine::scan::Row) {
        let pile = self.engine.scan(&self.root).expect("scan");
        let row = pile
            .row(path.as_bytes())
            .unwrap_or_else(|| panic!("{path} is not pending: [{}]", pile_string(&pile)))
            .clone();
        (Rendered::of(&row), row)
    }

    fn ledger(&self) -> &lastcall_engine::ledger::Ledger {
        &self.engine.root(&self.root).unwrap().ledger
    }

    fn store(&self) -> &lastcall_engine::store::Store {
        &self.engine.root(&self.root).unwrap().store
    }

    fn override_blob(&self, path: &str) -> Oid {
        let over = self
            .ledger()
            .overrides
            .get(path)
            .unwrap_or_else(|| panic!("override {path}: {:?}", self.ledger().overrides));
        over.blob
            .clone()
            .flatten()
            .unwrap_or_else(|| panic!("override {path} has no blob: {over:?}"))
    }

    fn blob_text(&self, oid: &Oid) -> String {
        String::from_utf8(self.store().cat_blob(oid).expect("cat blob")).expect("utf8")
    }

    fn seen_tree_entry(&self, path: &str) -> Oid {
        let seen = self.ledger().seen_tree.clone().expect("seen tree");
        let tree = self.store().ls_tree(&seen).expect("ls-tree");
        tree.get(path.as_bytes())
            .map(|(_, oid)| oid.clone())
            .unwrap_or_else(|| panic!("seen tree has no {path}: {tree:?}"))
    }

    fn assert_seen(&self, tree: &str, head: &str, label: &str) {
        let ledger = self.ledger();
        assert_eq!(
            ledger.seen_tree.as_ref().map(Oid::as_str),
            Some(tree),
            "{label}: seen_tree"
        );
        assert_eq!(
            ledger.seen_at.head_commit.as_ref().map(Oid::as_str),
            Some(head),
            "{label}: seen_at.head_commit"
        );
    }
}

/// `Accepted.pile` is the pile the next scan shows.
fn assert_accepted_pile(accepted: &Accepted, next: &Pile, label: &str) {
    assert_eq!(
        pile_string(&accepted.pile),
        pile_string(next),
        "{label}: Accepted.pile is the post-accept scan"
    );
}

#[test]
fn accept_loop_hunk_file_all_survives_the_agents_commits_and_shows_only_the_next_delta() {
    let agent = Agent::new();
    let state = TempDir::new("lc-loop-state");

    // First sight: the seed commit is the seen tree.
    let mut r = Reviewer::open(&agent, &state);
    let seed_commit = agent.repo.head().unwrap();
    let seed_tree = agent.tree_of_head();
    assert_pile!(r.engine, r.root, "", "first sight of a clean repo");
    r.assert_seen(&seed_tree, &seed_commit, "first sight");

    // The agent edits three files and commits one: HEAD moves, no baseline does.
    let f3_commit = agent.edit_three_commit_one();
    assert_ne!(f3_commit, seed_commit);
    let pile = assert_pile!(
        r.engine,
        r.root,
        "f1|f2|f3",
        "agent edited three, committed f3"
    );
    assert_eq!(pile.row(b"f1").unwrap().hunks.len(), 2, "f1 has two hunks");
    let rendered_f2 = pile.row(b"f2").unwrap().current.clone().unwrap().oid;
    let rendered_f3 = pile.row(b"f3").unwrap().current.clone().unwrap().oid;
    assert!(r.ledger().overrides.is_empty());
    r.assert_seen(&seed_tree, &seed_commit, "after the agent's commit");

    // Accept hunk 1 of f1: f1 shows hunk 2 only, against the hunk-one blob.
    let (rendered_f1, row_f1) = r.rendered("f1");
    let accepted = r.accept(AcceptRequest::Hunk {
        rendered: rendered_f1,
        hunks: row_f1.hunks.clone(),
        index: 0,
    });
    let pile = assert_pile!(r.engine, r.root, "f1|f2|f3", "hunk 1 of f1 accepted");
    assert_accepted_pile(&accepted, &pile, "accept hunk");
    let f1 = pile.row(b"f1").unwrap();
    assert_eq!(f1.hunks.len(), 1, "hunk 2 only: {:?}", f1.hunks);
    let hunk_one_blob = r.override_blob("f1");
    assert_eq!(r.blob_text(&hunk_one_blob), F1_HUNK_ONE_APPLIED);
    assert_eq!(
        f1.baseline.as_ref().map(|e| &e.oid),
        Some(&hunk_one_blob),
        "f1's baseline is the override"
    );
    assert_eq!(r.ledger().overrides.len(), 1);
    r.assert_seen(&seed_tree, &seed_commit, "after accept hunk");

    // Accept file f1: the override moves to the rendered blob; f2, f3 still pending.
    let (rendered_f1, _) = r.rendered("f1");
    let rendered_f1_oid = rendered_f1.oid.clone().unwrap();
    let accepted = r.accept(AcceptRequest::File(rendered_f1));
    let pile = assert_pile!(r.engine, r.root, "f2|f3", "f1 accepted");
    assert_accepted_pile(&accepted, &pile, "accept file");
    assert_eq!(r.override_blob("f1"), rendered_f1_oid);
    assert_eq!(r.blob_text(&rendered_f1_oid), F1_TWO_HUNKS);
    assert_eq!(r.ledger().overrides.len(), 1);
    r.assert_seen(&seed_tree, &seed_commit, "after accept file");

    // Accept all of the rest: overrides fold into a new seen tree, seen_at moves to HEAD.
    let accepted = r.accept(AcceptRequest::All(pile));
    let pile = assert_pile!(r.engine, r.root, "", "accept all -> empty");
    assert_accepted_pile(&accepted, &pile, "accept all");
    assert!(
        r.ledger().overrides.is_empty(),
        "overrides folded: {:?}",
        r.ledger().overrides
    );
    assert_eq!(
        r.seen_tree_entry("f1"),
        rendered_f1_oid,
        "folded f1 override"
    );
    assert_eq!(r.seen_tree_entry("f2"), rendered_f2, "snapshot f2");
    assert_eq!(r.seen_tree_entry("f3"), rendered_f3, "snapshot f3");
    let folded_tree = r.ledger().seen_tree.clone().unwrap();
    assert_ne!(folded_tree.as_str(), seed_tree);
    r.assert_seen(folded_tree.as_str(), &f3_commit, "after accept all");

    // The reviewer closes the TUI; the agent commits the rest; the reviewer reopens.
    drop(r);
    let rest_commit = agent.commit_the_rest();
    assert_ne!(rest_commit, f3_commit);
    let mut r = Reviewer::open(&agent, &state);
    assert_pile!(
        r.engine,
        r.root,
        "",
        "reopen after the agent's commit: still empty"
    );
    assert!(r.ledger().overrides.is_empty());
    r.assert_seen(
        folded_tree.as_str(),
        &f3_commit,
        "reopen: seen_at.head_commit unchanged by the agent's commit",
    );

    // The agent edits one file again: exactly that delta, against the folded blob.
    agent.edit_one_again();
    let pile = assert_pile!(r.engine, r.root, "f2", "one more edit -> that file only");
    let f2 = pile.row(b"f2").unwrap();
    assert_eq!(
        f2.baseline.as_ref().map(|e| &e.oid),
        Some(&rendered_f2),
        "baseline is the folded f2, not HEAD's"
    );
    assert_eq!((f2.added, f2.deleted), (1, 0), "only the new line");
    assert_eq!(f2.hunks.len(), 1);
    assert!(r.ledger().overrides.is_empty());
    r.assert_seen(folded_tree.as_str(), &f3_commit, "after the re-edit");
}

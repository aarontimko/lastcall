//! TUI snapshots: the sixteen Phase 3 scenes (its kickoff deliverable 8) and the Phase 4
//! accept scenes (kickoff deliverable 6), each rendered through
//! `ratatui::backend::TestBackend` from an `App` fed by a real engine over the shared
//! fixture, and pinned as two `insta` snapshots — the symbol frame (`*_frame`, the
//! backend's `Display`) and the style runs (`*_styles`, `render::styles`).
//!
//! The accept scenes drive the real `Engine::accept` from the reducer's own
//! `Effect::Accept` requests (built from the held rows, as the loop does) and feed the
//! results back through `App::accepted`.
//!
//! Every scene builds its own `fixture_parent` under a fresh temp dir (`<tmp>/W/`) with its
//! own state dir, so the mutating scenes never see each other and nothing touches the real
//! user state. Frames show basenames and root-relative paths only, never the temp path.
//! Refresh with `just snapshots-update`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lastcall::tui::app::{AcceptFailed, App, Effect, RootMeta, Selection};
use lastcall::tui::herdr::{Attention, Dot, HerdrUpdate, RootAgents, Scope};
use lastcall::tui::input::Action;
use lastcall::tui::render::{render, styles};
use lastcall_engine::engine::{Engine, EngineOptions};
use lastcall_engine::env::Env;
use lastcall_engine::ops::NoFault;
use lastcall_engine::scan::{Annotation, Change, Pile};
use lastcall_engine::watcher::EngineEvent;
use lastcall_testkit::engine::{open_engine, open_engine_with};
use lastcall_testkit::fixture_parent::{self, config};
use lastcall_testkit::fixture_repo::{FixtureRepo, engine_env_for};
use lastcall_testkit::tmp::TempDir;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const W: u16 = 100;
const H: u16 = 30;

struct Scene {
    _tmp: TempDir,
    parent: PathBuf,
    state: PathBuf,
    env: Env,
}

impl Scene {
    /// The three-root fixture (alpha, beta, notes) after `fixture_parent::build`.
    fn build() -> Scene {
        let tmp = TempDir::new("lc-tui");
        let parent = tmp.join("W");
        let state = tmp.join("state");
        let built = fixture_parent::build(&parent, &state).expect("fixture builds");
        let env = engine_env_for(&built.parent, &built.home, &state);
        Scene {
            _tmp: tmp,
            parent: built.parent,
            state,
            env,
        }
    }

    /// A parent holding one clean repo and nothing else.
    fn clean() -> Scene {
        let tmp = TempDir::new("lc-tui");
        let parent = tmp.join("W");
        let state = tmp.join("state");
        std::fs::create_dir_all(&parent).unwrap();
        FixtureRepo::new_in(TempDir::adopt(&parent), "solo").expect("repo");
        let home = state.join("home");
        let env = engine_env_for(&parent, &home, &state);
        Scene {
            _tmp: tmp,
            parent,
            state,
            env,
        }
    }

    fn engine(&self) -> Engine {
        open_engine(&self.parent, &self.env, &self.state, config())
    }

    fn engine_with(&self, options: EngineOptions) -> Engine {
        open_engine_with(&self.parent, &self.env, &self.state, config(), options)
    }

    fn repo(&self, name: &str) -> FixtureRepo {
        FixtureRepo::open_in(TempDir::adopt(&self.parent), name)
    }
}

fn root_named(engine: &Engine, name: &str) -> PathBuf {
    engine
        .root_paths()
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == name))
        .unwrap_or_else(|| panic!("root {name} is watched"))
}

/// Seed an app the way the loop does: `Resize`, `sync_roots`, then one `Pile` per root.
fn app_of(engine: &mut Engine) -> App {
    let mut app = App::new();
    app.handle(Action::Resize(W, H));
    let metas = engine.roots().into_iter().map(RootMeta::of).collect();
    app.sync_roots(metas);
    for (root, seq, result) in engine.scan_all() {
        let pile = result.expect("scan succeeds");
        app.apply(EngineEvent::Pile { root, seq, pile });
    }
    app
}

fn rescan(app: &mut App, engine: &mut Engine, root: &Path) -> Pile {
    let pile = engine.scan(root).expect("scan succeeds");
    app.apply(EngineEvent::Pile {
        root: root.to_path_buf(),
        seq: engine.scan_seq(),
        pile: pile.clone(),
    });
    pile
}

fn select_row(app: &mut App, root: &Path, path: &str) {
    app.select(Some(Selection::Row(
        root.to_path_buf(),
        path.as_bytes().to_vec(),
    )));
    assert!(
        app.selected_row().is_some(),
        "{path} is a pending row of {}",
        root.display()
    );
}

/// Mark the root's current pile as seen (`accept_all`), so later edits diff against it.
fn mark_seen(engine: &mut Engine, root: &Path) {
    let pile = engine.scan(root).expect("scan succeeds");
    engine
        .ops(root)
        .expect("ops")
        .accept_all(&pile, &NoFault)
        .expect("accept_all");
    let after = engine.scan(root).expect("scan succeeds");
    assert!(after.is_empty(), "everything seen: {after:?}");
}

fn draw(app: &App, w: u16, h: u16) -> (String, String) {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| {
            render(app, f);
        })
        .unwrap();
    let frame = terminal.backend().to_string();
    let style = styles(terminal.backend().buffer());
    (frame, style)
}

/// Pin `<name>_frame` and `<name>_styles`; the frame must also be reproducible.
fn snapshot(name: &str, app: &App, w: u16, h: u16) {
    let (frame, style) = draw(app, w, h);
    assert_eq!(draw(app, w, h).0, frame, "{name}: screen = f(App, area)");
    insta::assert_snapshot!(format!("{name}_frame"), frame);
    insta::assert_snapshot!(format!("{name}_styles"), style);
}

#[test]
fn tui_empty_state() {
    let scene = Scene::clean();
    let mut engine = scene.engine();
    let app = app_of(&mut engine);
    assert!(app.nav_entries().is_empty());
    snapshot("tui_empty_state", &app, W, H);
}

#[test]
fn tui_nav_three_roots() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    app.handle(Action::NavDown);
    app.handle(Action::NavDown);
    assert_eq!(
        app.selection,
        Some(Selection::Row(alpha.clone(), b"f1".to_vec()))
    );
    let (frame, _) = draw(&app, W, H);
    // An unchanged pile changes nothing: same selection, same cursor, same bytes. Only
    // the seq bookkeeping moves forward (the pile was current as of that scan).
    let mut before = app.clone();
    let pile = engine.scan(&alpha).expect("scan");
    let seq = engine.scan_seq();
    before.seq.insert(alpha.clone(), seq);
    let (changed, _) = app.apply(EngineEvent::Pile {
        root: alpha,
        seq,
        pile,
    });
    assert_eq!(changed, lastcall::tui::app::Changed::No);
    assert_eq!(app, before);
    assert_eq!(draw(&app, W, H).0, frame);
    snapshot("tui_nav_three_roots", &app, W, H);
}

#[test]
fn tui_nav_counts_update() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f2");
    scene
        .repo("alpha")
        .write("f2", "b\nagent edit\nsecond edit\n");
    let pile = rescan(&mut app, &mut engine, &alpha);
    assert_eq!(pile.row(b"f2").unwrap().added, 2);
    snapshot("tui_nav_counts_update", &app, W, H);
}

#[test]
fn tui_nav_full_paths() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    scene
        .repo("alpha")
        .write("docs/日本語.md", "# 日本語\n\nwide characters\n");
    rescan(&mut app, &mut engine, &alpha);
    select_row(&mut app, &alpha, "docs/日本語.md");
    app.handle(Action::ToggleFullPaths);
    assert!(app.full_paths);
    snapshot("tui_nav_full_paths", &app, W, H);
}

#[test]
fn tui_nav_remote() {
    let scene = Scene::build();
    scene
        .repo("alpha")
        .git(&[
            "remote",
            "set-url",
            "origin",
            "git@github.com:acme/alpha.git",
        ])
        .expect("set-url");
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    assert_eq!(app.roots[&alpha].meta.remote.as_deref(), Some("acme/alpha"));
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::ToggleRemote);
    snapshot("tui_nav_remote", &app, W, H);
}

#[test]
fn tui_diff_view_hunks() {
    let scene = Scene::build();
    let alpha_repo = scene.repo("alpha");
    let lines: Vec<String> = (1..=80).map(|i| format!("line {i}")).collect();
    alpha_repo.write("f1", format!("{}\n", lines.join("\n")));
    alpha_repo.git(&["add", "f1"]).unwrap();
    let mut alpha_repo = alpha_repo;
    alpha_repo.commit("agent: f1 to 80 lines").unwrap();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    mark_seen(&mut engine, &alpha);
    // Three one-line edits → three hunks of 3-context each (+3 −3). With git's 3-line
    // context hunk 2's header is diff line 9, so the cursor on it scrolls hunk 1 away.
    let mut edited = lines.clone();
    for i in [5, 45, 78] {
        edited[i - 1] = format!("LINE {i} (edited)");
    }
    alpha_repo.write("f1", format!("{}\n", edited.join("\n")));
    let mut app = app_of(&mut engine);
    let row = app.roots[&alpha].row(b"f1").expect("f1 pending").clone();
    assert_eq!(row.hunks.len(), 3, "{row:?}");
    assert_eq!((row.added, row.deleted), (3, 3));
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    app.handle(Action::HunkNext);
    assert_eq!(app.diff.hunk, 1);
    let offsets = lastcall::tui::app::hunk_offsets(&row.hunks);
    assert_eq!(
        app.diff.scroll, offsets[1],
        "header is the first visible line"
    );
    assert!(app.diff.scroll > 0, "view scrolled");
    snapshot("tui_diff_view_hunks", &app, W, H);
}

#[test]
fn tui_diff_view_deleted() {
    let scene = Scene::build();
    scene.repo("alpha").remove("f2");
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f2");
    assert_eq!(app.selected_row().unwrap().change, Change::Deleted);
    app.handle(Action::Open);
    snapshot("tui_diff_view_deleted", &app, W, H);
}

#[test]
fn tui_diff_view_mode_change() {
    let scene = Scene::build();
    // f3 is the seed file the fixture leaves untouched; f1 already has content edits.
    scene.repo("alpha").chmod_x("f3", true);
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f3");
    assert_eq!(app.selected_row().unwrap().change, Change::Mode);
    app.handle(Action::Open);
    snapshot("tui_diff_view_mode_change", &app, W, H);
}

#[test]
fn tui_diff_view_collapsed() {
    let scene = Scene::build();
    scene.repo("alpha").write(
        "Cargo.lock",
        "# This file is automatically @generated by Cargo.\n[[package]]\nname = \"alpha\"\n",
    );
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "Cargo.lock");
    assert!(app.selected_row().unwrap().collapsed.is_some());
    app.handle(Action::Open);
    snapshot("tui_diff_view_collapsed", &app, W, H);
}

#[test]
fn tui_diff_view_unreadable() {
    let scene = Scene::build();
    let alpha_repo = scene.repo("alpha");
    alpha_repo.remove("f2");
    std::fs::create_dir(alpha_repo.path().join("f2")).unwrap();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f2");
    let row = app.selected_row().unwrap();
    assert!(
        matches!(row.change, Change::Unreadable | Change::Typechange),
        "{row:?}"
    );
    app.handle(Action::Open);
    snapshot("tui_diff_view_unreadable", &app, W, H);
}

#[test]
fn tui_diff_view_rename() {
    let scene = Scene::build();
    let mut alpha_repo = scene.repo("alpha");
    alpha_repo
        .commit_files(
            &[("d/old.rs", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n")],
            "old",
        )
        .unwrap();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    mark_seen(&mut engine, &alpha);
    alpha_repo.remove("d/old.rs");
    alpha_repo.write("d/new.rs", "l1\nL2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n");
    let mut app = app_of(&mut engine);
    select_row(&mut app, &alpha, "d/new.rs");
    assert!(app.selected_row().unwrap().rename.is_some());
    app.handle(Action::Open);
    snapshot("tui_diff_view_rename", &app, W, H);
}

#[test]
fn tui_group_view() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let beta = root_named(&engine, "beta");
    app.select(Some(Selection::Group(beta.clone(), Annotation::Upstream)));
    assert!(app.roots[&beta].group(Annotation::Upstream).is_some());
    app.handle(Action::Open);
    snapshot("tui_group_view", &app, W, H);
}

#[test]
fn tui_help_overlay() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    app.handle(Action::NavDown);
    app.handle(Action::NavDown);
    app.handle(Action::Help);
    assert!(app.help);
    snapshot("tui_help_overlay", &app, W, H);
}

#[test]
fn tui_narrow_60x20() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Resize(60, 20));
    snapshot("tui_narrow_60x20", &app, 60, 20);
}

#[test]
fn tui_too_small_30x8() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    app.handle(Action::Resize(30, 8));
    snapshot("tui_too_small_30x8", &app, 30, 8);
}

#[test]
fn tui_status_line_head_notice() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    let head = app.roots[&alpha].meta.head.clone();
    let (_, effect) = app.apply(EngineEvent::Head {
        root: alpha,
        from: head.clone(),
        to: head,
        branch: Some("main".into()),
        notice: Some("committed on main (1 commit)".into()),
    });
    assert_eq!(effect, Some(lastcall::tui::app::Effect::SyncRoots));
    let at = app.status.as_ref().unwrap().at;
    app.now = at;
    assert_eq!(app.status_age().as_deref(), Some("0s"));
    snapshot("tui_status_line_head_notice", &app, W, H);
}

/// Re-record `tests/fixtures/piles_three_roots.json` (the reducer unit tests' pile fixture)
/// from a fresh `fixture_parent::build`:
/// `cargo test -p lastcall --test test_e2e_tui_snapshots record_pile_fixture -- --ignored`
#[test]
#[ignore]
fn record_pile_fixture() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut piles: BTreeMap<String, Pile> = BTreeMap::new();
    for (root, _seq, result) in engine.scan_all() {
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        piles.insert(name, result.expect("scan ok"));
    }
    let out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/piles_three_roots.json");
    std::fs::write(&out, serde_json::to_string_pretty(&piles).unwrap()).unwrap();
}

// ---- Phase 4: accept (kickoff deliverable 6) ---------------------------------------------

/// Run the reducer's accept effect against the engine the way the loop does — every
/// request through `Engine::accept`, errors as their text — and feed the results back.
fn run_accept(app: &mut App, engine: &mut Engine, effect: Option<Effect>) {
    let Some(Effect::Accept(reqs)) = effect else {
        panic!("an accept effect, got {effect:?}");
    };
    let results = reqs
        .into_iter()
        .map(|(root, req)| {
            let result = engine.accept(&root, req).map_err(|e| AcceptFailed::of(&e));
            (root, result)
        })
        .collect();
    app.accepted(results);
}

fn status_text(app: &App) -> &str {
    app.status.as_ref().map(|s| s.text.as_str()).unwrap_or("")
}

/// alpha with `f1` committed at 80 lines and marked seen, then lines `edits` changed:
/// one hunk per edit (3-line context). `f2` is edited too when `f2_pending`.
fn alpha_hunks(scene: &Scene, edits: &[usize], f2_pending: bool) -> (Engine, PathBuf) {
    let alpha_repo = scene.repo("alpha");
    let lines: Vec<String> = (1..=80).map(|i| format!("line {i}")).collect();
    alpha_repo.write("f1", format!("{}\n", lines.join("\n")));
    alpha_repo.git(&["add", "f1"]).unwrap();
    let mut alpha_repo = alpha_repo;
    alpha_repo.commit("agent: f1 to 80 lines").unwrap();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    mark_seen(&mut engine, &alpha);
    let mut edited = lines.clone();
    for i in edits {
        edited[i - 1] = format!("LINE {i} (edited)");
    }
    alpha_repo.write("f1", format!("{}\n", edited.join("\n")));
    if f2_pending {
        alpha_repo.write("f2", "b\nagent edit after seen\n");
    }
    (engine, alpha)
}

#[test]
fn tui_accept_controls() {
    let scene = Scene::build();
    let (mut engine, alpha) = alpha_hunks(&scene, &[5, 45], false);
    let mut app = app_of(&mut engine);
    let row = app.roots[&alpha].row(b"f1").expect("f1 pending").clone();
    assert_eq!(row.hunks.len(), 2, "{row:?}");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("[Accept All]"), "{frame}");
    assert!(frame.contains("[A accept file]"), "{frame}");
    assert_eq!(frame.matches("[a accept]").count(), 2, "{frame}");
    assert!(
        frame.contains("a accept hunk  A accept file  ^A accept all"),
        "{frame}"
    );
    snapshot("tui_accept_controls", &app, W, H);
}

/// §6.7: accepting the last hunk of a file advances to the next file. Three hunks
/// accepted one by one from the cursor (index 0 each time: the next hunk slides into it);
/// the third leaves `f1` clean and the selection lands on `f2`.
#[test]
fn tui_accept_last_hunk_advances() {
    let scene = Scene::build();
    let (mut engine, alpha) = alpha_hunks(&scene, &[5, 45, 78], true);
    let mut app = app_of(&mut engine);
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    assert_eq!(app.roots[&alpha].row(b"f1").unwrap().hunks.len(), 3);

    let (_, effect) = app.handle(Action::Accept);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted f1 · 2 hunks left");
    assert_eq!(app.roots[&alpha].row(b"f1").unwrap().hunks.len(), 2);
    assert_eq!(
        app.selection,
        Some(Selection::Row(alpha.clone(), b"f1".to_vec())),
        "hunks remain: the cursor stays on f1"
    );
    assert_eq!(app.diff.hunk, 0);

    let (_, effect) = app.handle(Action::Accept);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted f1 · 1 hunk left");
    assert_eq!(app.roots[&alpha].row(b"f1").unwrap().hunks.len(), 1);

    let (_, effect) = app.handle(Action::Accept);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted f1 · file complete");
    assert!(app.roots[&alpha].row(b"f1").is_none(), "f1 is clean");
    assert_eq!(
        app.selection,
        Some(Selection::Row(alpha.clone(), b"f2".to_vec())),
        "advanced to the next file"
    );
    assert!(app.accepting.is_none());
    snapshot("tui_accept_last_hunk_advances", &app, W, H);
}

/// §6.7: accepting a repo's last file collapses the repo out of the nav. alpha's `f1`
/// then `f2` accepted whole; alpha unlists and the selection moves to beta's first row.
#[test]
fn tui_accept_last_file_collapses_repo() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    let beta = root_named(&engine, "beta");
    select_row(&mut app, &alpha, "f1");

    let (_, effect) = app.handle(Action::AcceptFile);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted f1");
    assert_eq!(
        app.selection,
        Some(Selection::Row(alpha.clone(), b"f2".to_vec()))
    );

    let (_, effect) = app.handle(Action::AcceptFile);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted f2");
    assert!(
        !app.roots[&alpha].listed(),
        "alpha collapsed out of the nav"
    );
    assert_eq!(
        app.selection,
        Some(Selection::Row(beta.clone(), b"u1".to_vec())),
        "the next listed root's first row"
    );
    assert!(engine.scan(&alpha).expect("scan").is_empty());
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("lastcall  2 repos · 3 files"), "{frame}");
    assert!(!frame.contains("alpha"), "{frame}");
    snapshot("tui_accept_last_file_collapses_repo", &app, W, H);
}

/// §7.2 CAS refusal on screen: the request is built from the held row, the file changes
/// underneath before the engine runs it, the engine refuses; the status line carries the
/// `Refused` text and the row stays, now showing the newer content.
#[test]
fn tui_accept_refused() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    let (_, effect) = app.handle(Action::AcceptFile);
    assert!(app.accepting.is_some());
    let alpha_repo = scene.repo("alpha");
    let f1 = alpha_repo.path().join("f1");
    let mut text = std::fs::read_to_string(&f1).unwrap();
    text.push_str("another edit, after the frame\n");
    alpha_repo.write("f1", text);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(
        status_text(&app),
        "f1: changed since rendered; not accepted"
    );
    assert!(app.accepting.is_none());
    let row = app.roots[&alpha].row(b"f1").expect("f1 still pending");
    assert_eq!(
        (row.added, row.deleted),
        (2, 1),
        "the pile shows the newer content: {row:?}"
    );
    assert_eq!(
        app.selection,
        Some(Selection::Row(alpha.clone(), b"f1".to_vec())),
        "a refusal does not advance"
    );
    snapshot("tui_accept_refused", &app, W, H);
}

/// beta with 11 pending files, one of them the upstream group's `u1`: `a` on the root
/// entry asks first, with the grouped/collapsed line. A 12th file's pile applied
/// underneath shows `12 files` in the same modal (second frame, `_live`).
#[test]
fn tui_accept_all_confirm() {
    let scene = Scene::build();
    let beta_repo = scene.repo("beta");
    for i in 1..=9 {
        beta_repo.write(&format!("g{i:02}"), format!("generated {i}\n"));
    }
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let beta = root_named(&engine, "beta");
    assert_eq!(app.roots[&beta].rows().len(), 11);
    assert_eq!(
        app.roots[&beta].row(b"u1").unwrap().annotation,
        Some(Annotation::Upstream)
    );
    app.select(Some(Selection::Root(beta.clone())));
    let (changed, effect) = app.handle(Action::Accept);
    assert_eq!(changed, lastcall::tui::app::Changed::Yes);
    assert_eq!(effect, None, "asks first");
    assert!(app.confirm.is_some());
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("Accept all 11 files in beta?"), "{frame}");
    assert!(
        frame.contains("1 grouped upstream · 0 collapsed"),
        "{frame}"
    );
    assert!(frame.contains("y / ⏎ confirm    n / Esc cancel"), "{frame}");
    snapshot("tui_accept_all_confirm", &app, W, H);

    beta_repo.write("g10", "generated 10\n");
    let pile = rescan(&mut app, &mut engine, &beta);
    assert_eq!(pile.rows.len(), 12);
    assert!(app.confirm.is_some(), "the modal stays open");
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("Accept all 12 files in beta?"), "{frame}");
    snapshot("tui_accept_all_confirm_live", &app, W, H);

    // Confirm folds what is shown now: all twelve.
    let (_, effect) = app.handle(Action::Confirm);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted 12 files in beta");
    assert!(!app.roots[&beta].listed());
    assert!(engine.scan(&beta).expect("scan").is_empty());
}

/// G0 Q5: exactly 10 files accept without asking. Only alpha is pending (beta and notes
/// marked seen), with `f1`, `f2` and eight generated files; `ctrl-a` folds it at once.
#[test]
fn tui_accept_all_no_confirm_at_10() {
    let scene = Scene::build();
    let alpha_repo = scene.repo("alpha");
    for i in 1..=8 {
        alpha_repo.write(&format!("g{i:02}"), format!("generated {i}\n"));
    }
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    for name in ["beta", "notes"] {
        let root = root_named(&engine, name);
        mark_seen(&mut engine, &root);
    }
    let mut app = app_of(&mut engine);
    assert_eq!(app.listed_roots().count(), 1);
    assert_eq!(app.roots[&alpha].rows().len(), 10);
    let (_, effect) = app.handle(Action::AcceptAll);
    assert!(app.confirm.is_none(), "ten files ask nothing");
    assert!(app.accepting.is_some());
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted 10 files in alpha");
    assert!(app.listed_roots().next().is_none());
    assert_eq!(app.selection, None);
    assert!(engine.scan(&alpha).expect("scan").is_empty());
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("nothing pending across 3 roots"), "{frame}");
    snapshot("tui_accept_all_no_confirm_at_10", &app, W, H);
}

/// Ruling 1: an engine capped at 3 rows over alpha with 5 pending files shows the first
/// three by path, `3+ files` in the nav and header, and the notice under the root's
/// main-view header.
#[test]
fn tui_row_cap_notice() {
    let scene = Scene::build();
    let alpha_repo = scene.repo("alpha");
    for i in 1..=3 {
        alpha_repo.write(&format!("g{i}"), format!("generated {i}\n"));
    }
    let mut engine = scene.engine_with(EngineOptions {
        row_cap: 3,
        ..EngineOptions::default()
    });
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    let view = &app.roots[&alpha];
    assert_eq!(view.pile.omitted, 2, "{:?}", view.pile);
    assert_eq!(
        view.rows()
            .iter()
            .map(|r| r.path_lossy())
            .collect::<Vec<_>>(),
        ["f1", "f2", "g1"]
    );
    assert!(app.any_truncated());
    app.select(Some(Selection::Root(alpha.clone())));
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("lastcall  3 repos · 6+ files"), "{frame}");
    assert!(frame.contains("main · 3+ files"), "{frame}");
    assert!(
        frame.contains("3 files shown · 2 more changed paths not scanned (first 3 by path)"),
        "{frame}"
    );
    snapshot("tui_row_cap_notice", &app, W, H);
}

// --- herdr overlay (kickoff deliverables 5 and 8) ---------------------------------------

/// Fold a derived association in the way the loop's fourth `select!` arm does.
fn herdr_roots(app: &mut App, derived: &[(&Path, Attention, &str)]) {
    let map: BTreeMap<PathBuf, RootAgents> = derived
        .iter()
        .map(|(root, status, agent)| {
            (
                root.to_path_buf(),
                RootAgents {
                    status: *status,
                    agents: 1,
                    pane: Some(format!("w1:{agent}")),
                    agent: Some((*agent).to_owned()),
                },
            )
        })
        .collect();
    app.handle(Action::Herdr(HerdrUpdate::Roots(map)));
}

fn herdr_connected(app: &mut App) {
    app.handle(Action::Herdr(HerdrUpdate::Connected {
        version: "0.8.2".to_owned(),
        protocol: 21,
    }));
}

/// A dot per state: alpha's agent is done (bright flag), beta's is blocked (red), notes'
/// is working (yellow). The count follows the name when a root has more than one agent.
#[test]
fn tui_herdr_status_dots() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let (alpha, beta, notes) = (
        root_named(&engine, "alpha"),
        root_named(&engine, "beta"),
        root_named(&engine, "notes"),
    );
    herdr_connected(&mut app);
    herdr_roots(
        &mut app,
        &[
            (&alpha, Attention::Done, "claude"),
            (&beta, Attention::Blocked, "codex"),
            (&notes, Attention::Working, "claude"),
        ],
    );
    assert_eq!(app.herdr.dot(&alpha), Some(Dot::Ready { acked: false }));
    assert_eq!(app.herdr.dot(&beta), Some(Dot::Blocked));
    assert_eq!(app.herdr.dot(&notes), Some(Dot::Working));
    snapshot("tui_herdr_status_dots", &app, W, H);
}

/// Ruling 10: `d` dims alpha's flag and nothing else moves — herdr still says `done`, the
/// repo stays listed, and beta's blocked dot is untouched.
#[test]
fn tui_herdr_ready_ack_dims() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let (alpha, beta) = (root_named(&engine, "alpha"), root_named(&engine, "beta"));
    herdr_connected(&mut app);
    herdr_roots(
        &mut app,
        &[
            (&alpha, Attention::Done, "claude"),
            (&beta, Attention::Blocked, "codex"),
        ],
    );
    app.select(Some(Selection::Root(alpha.clone())));
    let (changed, effect) = app.handle(Action::Ack);
    assert_eq!(changed, lastcall::tui::app::Changed::Yes);
    assert!(matches!(effect, Some(Effect::Toast(_))), "{effect:?}");
    assert_eq!(app.herdr.dot(&alpha), Some(Dot::Ready { acked: true }));
    assert_eq!(app.herdr.dot(&beta), Some(Dot::Blocked));
    snapshot("tui_herdr_ready_ack_dims", &app, W, H);
}

/// Ruling 4: alpha has nothing pending, and herdr's `done` lists it anyway with a line
/// that says why. beta's `working` is not a reason to list it, so it stays off.
#[test]
fn tui_herdr_flag_only_root_listed() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let (alpha, beta) = (root_named(&engine, "alpha"), root_named(&engine, "beta"));
    mark_seen(&mut engine, &alpha);
    mark_seen(&mut engine, &beta);
    let mut app = app_of(&mut engine);
    assert!(
        !app.listed_roots().any(|v| v.meta.path == alpha),
        "nothing pending, nothing listed"
    );
    herdr_connected(&mut app);
    herdr_roots(
        &mut app,
        &[
            (&alpha, Attention::Done, "claude"),
            (&beta, Attention::Working, "codex"),
        ],
    );
    assert!(app.listed_roots().any(|v| v.meta.path == alpha));
    assert!(
        !app.listed_roots().any(|v| v.meta.path == beta),
        "working only annotates (ruling 4)"
    );
    app.select(Some(Selection::Root(alpha)));
    let (frame, _) = draw(&app, W, H);
    assert!(
        frame.contains(&lastcall::tui::render::nothing_pending("done")),
        "{frame}"
    );
    snapshot("tui_herdr_flag_only_root_listed", &app, W, H);
}

/// The four header badges, each pinned as the header line it produces: connected names
/// the version, reconnecting is undimmed so it is visible, and a standalone that the user
/// asked for (`mode = "on"`) says why while the default one stays quiet.
#[test]
fn tui_herdr_header_states() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let base = app_of(&mut engine);
    let states: Vec<(&str, HerdrUpdate)> = vec![
        (
            "connected",
            HerdrUpdate::Connected {
                version: "0.8.2".to_owned(),
                protocol: 21,
            },
        ),
        ("reconnecting", HerdrUpdate::Reconnecting),
        (
            "standalone (auto)",
            HerdrUpdate::Standalone {
                reason: String::new(),
            },
        ),
        (
            "standalone (on)",
            HerdrUpdate::Standalone {
                reason: "no herdr socket".to_owned(),
            },
        ),
    ];
    let mut lines = String::new();
    for (name, update) in states {
        let mut app = base.clone();
        app.handle(Action::Herdr(update));
        let (frame, _) = draw(&app, W, H);
        let header = frame.lines().next().expect("a header").trim_end();
        lines.push_str(&format!("{name:<18} |{header}|\n"));
    }
    insta::assert_snapshot!("tui_herdr_header_states", lines);
}

/// Deliverable 8: the scope hides what is not in this workspace, and the notice that says
/// so is mandatory — it is right-aligned beside the hint line, naming the count and the
/// key that shows everything again.
#[test]
fn tui_herdr_scope_notice() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    herdr_connected(&mut app);
    app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
        label: "alpha".to_owned(),
        roots: [alpha.clone()].into_iter().collect(),
    }))));
    app.herdr.scoped = true;
    assert_eq!(app.listed_roots().count(), 1);
    assert_eq!(
        app.scope_notice().as_deref(),
        Some("scope: alpha · 2 repos hidden (w shows all)")
    );
    snapshot("tui_herdr_scope_notice", &app, W, H);
}

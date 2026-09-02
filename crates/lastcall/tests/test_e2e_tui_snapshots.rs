//! Phase 3 TUI snapshots (kickoff deliverable 8): sixteen scenes, each rendered through
//! `ratatui::backend::TestBackend` from an `App` fed by a real engine over the shared
//! fixture, and pinned as two `insta` snapshots — the symbol frame (`*_frame`, the
//! backend's `Display`) and the style runs (`*_styles`, `render::styles`).
//!
//! Every scene builds its own `fixture_parent` under a fresh temp dir (`<tmp>/W/`) with its
//! own state dir, so the mutating scenes never see each other and nothing touches the real
//! user state. Frames show basenames and root-relative paths only, never the temp path.
//! Refresh with `just snapshots-update`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lastcall::tui::app::{App, RootMeta, Selection};
use lastcall::tui::input::Action;
use lastcall::tui::render::{render, styles};
use lastcall_engine::engine::Engine;
use lastcall_engine::env::Env;
use lastcall_engine::ops::NoFault;
use lastcall_engine::scan::{Annotation, Change, Pile};
use lastcall_engine::watcher::EngineEvent;
use lastcall_testkit::engine::open_engine;
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
    for (root, result) in engine.scan_all() {
        let pile = result.expect("scan succeeds");
        app.apply(EngineEvent::Pile { root, pile });
    }
    app
}

fn rescan(app: &mut App, engine: &mut Engine, root: &Path) -> Pile {
    let pile = engine.scan(root).expect("scan succeeds");
    app.apply(EngineEvent::Pile {
        root: root.to_path_buf(),
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
    // An unchanged pile changes nothing: same selection, same cursor, same bytes.
    let before = app.clone();
    let pile = engine.scan(&alpha).expect("scan");
    let (changed, _) = app.apply(EngineEvent::Pile { root: alpha, pile });
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
    let mut edited = lines.clone();
    for i in 5..=25 {
        edited[i - 1] = format!("LINE {i} (edited)");
    }
    edited[44] = "LINE 45 (edited)".into();
    edited[77] = "LINE 78 (edited)".into();
    alpha_repo.write("f1", format!("{}\n", edited.join("\n")));
    let mut app = app_of(&mut engine);
    let row = app.roots[&alpha].row(b"f1").expect("f1 pending").clone();
    assert_eq!(row.hunks.len(), 3, "{row:?}");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    app.handle(Action::HunkNext);
    assert_eq!(app.diff.hunk, 1);
    assert!(app.diff.scroll > 28, "hunk 2 starts at {}", app.diff.scroll);
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
    for (root, result) in engine.scan_all() {
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        piles.insert(name, result.expect("scan ok"));
    }
    let out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/piles_three_roots.json");
    std::fs::write(&out, serde_json::to_string_pretty(&piles).unwrap()).unwrap();
}

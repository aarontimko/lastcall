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

use lastcall::tui::app::{AcceptFailed, App, Changed, Effect, FlagKind, RootMeta, Selection};
use lastcall::tui::herdr::{AgentCandidate, Attention, Dot, HerdrUpdate, RootAgents, Scope};
use lastcall::tui::input::{Action, EditKey, EditorKey, NoteKey, PickKey};
use lastcall::tui::render::{render, styles};
use lastcall_engine::engine::{Engine, EngineOptions, SaveRequest};
use lastcall_engine::env::Env;
use lastcall_engine::ops::NoFault;
use lastcall_engine::scan::{Annotation, Change, Collapsed, Pile};
use lastcall_engine::watcher::EngineEvent;
use lastcall_testkit::engine::{open_engine, open_engine_with};
use lastcall_testkit::fixture_parent::{self, config, draft_config};
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
    /// What `fixture_parent::build` created; `None` for [`Scene::clean`], which has no
    /// alpha/beta/notes to hand `add_draft_root`.
    built: Option<fixture_parent::Built>,
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
            parent: built.parent.clone(),
            state,
            env,
            built: Some(built),
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
            built: None,
        }
    }

    fn engine(&self) -> Engine {
        open_engine(&self.parent, &self.env, &self.state, config())
    }

    fn engine_with(&self, options: EngineOptions) -> Engine {
        open_engine_with(&self.parent, &self.env, &self.state, config(), options)
    }

    /// An engine over the **four**-root config: only for a scene that called
    /// [`fixture_parent::add_draft_root`] (Phase 6 deliverable 1(c)).
    fn engine_with_draft_root(&self) -> Engine {
        open_engine(&self.parent, &self.env, &self.state, draft_config())
    }

    /// The scene-owned fourth root, first-sighted at `baseline`; see
    /// [`fixture_parent::add_draft_root`] for why it is never in the shared fixture.
    fn add_draft_root(&self, baseline: &str) -> PathBuf {
        let built = self.built.as_ref().expect("a Scene::build fixture");
        fixture_parent::add_draft_root(built, &self.state, baseline).expect("the fourth root")
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

/// §6.7 (Amendment v1.9): a repo with nothing pending is a nav row, not an absence — the
/// one clean repo here is a dim name-and-branch row with no file rows under it. The
/// `None` arm's `nothing pending across N repos` is what a frame with no *pending* row
/// says — the one repo here is empty, so it says it whether or not `t` has taken the row
/// off the nav (verifier (a) F5).
#[test]
fn tui_empty_state() {
    let scene = Scene::clean();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    assert_eq!(app.nav_entries().len(), 1, "the repo row is the nav");
    assert!(app.roots.values().all(|v| !v.listed()), "nothing pending");
    snapshot("tui_empty_state", &app, W, H);

    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("nothing pending across 1 repo"), "{frame}");

    app.handle(Action::HideEmpty);
    assert!(app.nav_entries().is_empty());
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("nothing pending across 1 repo"), "{frame}");
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

/// Phase 6 deliverable 1(c): the diff view of a **draft root's** two-hunk row, so one
/// frame shows hunks under a `draft`-labelled root (`tui_nav_three_roots` already shows a
/// pending draft root beside a git root, but with no hunks open). The fourth root is the
/// scene's own — `fixture_parent::build` and its three-root assertion are untouched, so
/// every other snapshot still reads `3 repos`.
#[test]
fn tui_draft_root_hunks() {
    let scene = Scene::build();
    let baseline: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let drafts = scene.add_draft_root(&baseline);
    // The agent edits two lines six apart: two hunks at CONTEXT 3, not one merged hunk.
    let edited: String = (1..=20)
        .map(|i| match i {
            2 | 18 => format!("line {i} edited by the agent\n"),
            _ => format!("line {i}\n"),
        })
        .collect();
    std::fs::write(drafts.join("reply.md"), edited).expect("the agent's edit");

    let mut engine = scene.engine_with_draft_root();
    let mut app = app_of(&mut engine);
    select_row(&mut app, &drafts, "reply.md");
    let row = app.selected_row().expect("the draft row");
    assert_eq!(row.hunks.len(), 2, "two hunks under the draft root");
    assert_eq!(row.collapsed, None, "a small text draft is not collapsed");
    assert_eq!(
        app.roots[&drafts].meta.branch, None,
        "a draft root has no branch; the label line reads `draft`"
    );
    app.handle(Action::Open);
    snapshot("tui_draft_root_hunks", &app, W, H);
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

/// Phase 6 gate item 2(b): `npm install` rewrites a 4,000-line lockfile and the reviewer
/// sees **one** row with live four-digit counts, under the frozen default `collapsed_globs`
/// (no config override), plus the collapsed diff view for it.
#[test]
fn tui_nav_collapsed_lockfile() {
    let before: String = (0..4_000)
        .map(|i| format!("    \"pkg-{i}\": {{ \"version\": \"1.0.{i}\" }},\n"))
        .collect();
    let scene = Scene::build();
    let mut alpha_repo = scene.repo("alpha");
    alpha_repo
        .commit_files(&[("package-lock.json", before.as_str())], "lock")
        .unwrap();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    mark_seen(&mut engine, &alpha);

    // Two thirds of the versions move and 213 packages are added: the counts are large,
    // four digits, and deliberately unequal so the frame proves both are live.
    let after: String = (0..4_000)
        .map(|i| {
            let v = if i % 3 == 0 { "1.0" } else { "2.4" };
            format!("    \"pkg-{i}\": {{ \"version\": \"{v}.{i}\" }},\n")
        })
        .chain((0..213).map(|i| format!("    \"new-{i}\": {{ \"version\": \"1.0.0\" }},\n")))
        .collect();
    assert!(
        after.len() < 512 * 1024,
        "under collapse_size_bytes, so the glob is the reason: {}",
        after.len()
    );
    alpha_repo.write("package-lock.json", &after);

    let mut app = app_of(&mut engine);
    select_row(&mut app, &alpha, "package-lock.json");
    let row = app.selected_row().unwrap();
    assert_eq!(row.collapsed, Some(Collapsed::Glob));
    assert!(row.hunks.is_empty(), "a collapsed row carries no hunks");
    assert!(row.added > 2_000 && row.deleted > 2_000 && row.added != row.deleted);
    app.handle(Action::Open);
    snapshot("tui_nav_collapsed_lockfile", &app, W, H);
}

/// Phase 6 gate item 3(b): the two size/binary collapse classes at the **frozen default**
/// `collapse_size_bytes` (512 KiB), with the boundary row beside them — 524,288 bytes is
/// not collapsed and keeps its hunks, 524,289 is `Size`, the PNG is `Binary`. The diff view
/// is on the binary row.
#[test]
fn tui_nav_collapsed_binary_and_size() {
    const LIMIT: usize = 512 * 1024;
    // 32 bytes per line, so LIMIT is a whole number of lines.
    let line = |c: char| format!("{}\n", std::iter::repeat_n(c, 31).collect::<String>());
    let at_limit: String = std::iter::repeat_n(line('a'), LIMIT / 32).collect();
    assert_eq!(at_limit.len(), LIMIT);
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.resize(2 * 1024 * 1024, b'\x42');

    let scene = Scene::build();
    let mut alpha_repo = scene.repo("alpha");
    alpha_repo
        .commit_files(
            &[
                ("img.png", "placeholder\n"),
                ("at_limit.txt", at_limit.as_str()),
                ("over_limit.txt", at_limit.as_str()),
            ],
            "seed",
        )
        .unwrap();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    assert_eq!(engine.config().collapse_size_bytes, LIMIT as u64);
    mark_seen(&mut engine, &alpha);

    alpha_repo.write("img.png", &png);
    let mut changed_at_limit = at_limit.clone();
    changed_at_limit.replace_range(0..32, &line('b'));
    alpha_repo.write("at_limit.txt", &changed_at_limit);
    alpha_repo.write("over_limit.txt", format!("{at_limit}x"));

    let mut app = app_of(&mut engine);
    select_row(&mut app, &alpha, "at_limit.txt");
    let boundary = app.selected_row().unwrap();
    assert_eq!(
        boundary.collapsed, None,
        "524,288 bytes is not over the limit"
    );
    assert!(
        !boundary.hunks.is_empty(),
        "the boundary row keeps its hunks"
    );
    select_row(&mut app, &alpha, "over_limit.txt");
    assert_eq!(
        app.selected_row().unwrap().collapsed,
        Some(Collapsed::Size),
        "one byte over"
    );
    select_row(&mut app, &alpha, "img.png");
    assert_eq!(
        app.selected_row().unwrap().collapsed,
        Some(Collapsed::Binary)
    );
    app.handle(Action::Open);
    snapshot("tui_nav_collapsed_binary_and_size", &app, W, H);
}

/// Phase 6 deliverable 4: the same collapsed row after `e`. The header keeps the collapsed
/// summary and its `[e expand]` control; under it are the real hunks the engine computed on
/// demand, and the cap footer says what the 2,000-line budget dropped. The expansion is fed
/// in exactly as the loop does it — `Effect::Expand` → `Engine::hunks_of` → `set_expanded`.
#[test]
fn tui_diff_view_collapsed_expanded() {
    let before: String = (0..1_400)
        .map(|i| format!("    \"pkg-{i}\": {{ \"version\": \"1.0.{i}\" }},\n"))
        .collect();
    let scene = Scene::build();
    let mut alpha_repo = scene.repo("alpha");
    alpha_repo
        .commit_files(&[("package-lock.json", before.as_str())], "lock")
        .unwrap();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    mark_seen(&mut engine, &alpha);
    let after: String = (0..1_400)
        .map(|i| format!("    \"pkg-{i}\": {{ \"version\": \"2.4.{i}\" }},\n"))
        .collect();
    alpha_repo.write("package-lock.json", &after);

    let mut app = app_of(&mut engine);
    select_row(&mut app, &alpha, "package-lock.json");
    let row = app.selected_row().unwrap().clone();
    assert_eq!(row.collapsed, Some(Collapsed::Glob));
    let (changed, effect) = app.handle(Action::Expand);
    assert_eq!(changed, Changed::No, "asking for hunks draws nothing");
    let Some(Effect::Expand(root, asked)) = effect else {
        panic!("an expand effect: {effect:?}");
    };
    let view = engine.hunks_of(&root, &asked).expect("the expansion");
    assert!(view.omitted_lines > 0, "a whole-file rewrite hits the cap");
    assert_eq!(app.set_expanded(root, &asked, view), Changed::Yes);
    app.handle(Action::Open);
    snapshot("tui_diff_view_collapsed_expanded", &app, W, H);
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

/// The same overlay with room to breathe: one column, every row on its own line.
///
/// The two-column form (deliverable 8) is a response to a terminal too short to hold the
/// rows, so the tall terminal is the control — it proves the layout switched for the reason
/// claimed and not because the table grew.
#[test]
fn tui_help_overlay_tall() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    app.handle(Action::NavDown);
    app.handle(Action::NavDown);
    app.handle(Action::Help);
    assert!(app.help);
    snapshot("tui_help_overlay_tall", &app, W, 45);
}

/// The same overlay with room for neither form: 80 columns is the standard width and this
/// keymap needs 100 for two columns, so the body is one column and it clips.
///
/// Design pass D12 (ruling R12): the clip **says so**. The last body row above the pinned
/// `quit` is a dim `… N more keys (100 columns shows all)` — the count is the rows that are
/// not on the frame, the column figure is computed from the width two columns would need,
/// and the footer (the newline note, the mouse note, `any key closes`) is never what the
/// clip spends.
#[test]
fn tui_help_overlay_80x24() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    app.handle(Action::NavDown);
    app.handle(Action::NavDown);
    app.handle(Action::Help);
    assert!(app.help);
    let (frame, _) = draw(&app, 80, 24);
    let lines: Vec<&str> = frame.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains("more key"))
        .unwrap_or_else(|| panic!("a clip notice:\n{frame}"));
    assert!(lines[at].contains("columns shows all"), "{}", lines[at]);
    assert!(
        lines[at + 1].contains("quit"),
        "the notice sits directly above the pinned quit:\n{frame}"
    );
    assert!(frame.contains("any key closes"), "{frame}");
    snapshot("tui_help_overlay_80x24", &app, 80, 24);
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
    // Deliverable 4: at 100 columns `^A accept all` has already gone — the header's
    // `[Accept All]` on the same frame says it, and `t hide empty` outlives it.
    assert!(
        frame.contains("a accept hunk  A accept file  t hide empty"),
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

/// §6.7 (Amendment v1.9): accepting a repo's last file lands on the repo's own name row.
/// alpha's `f1`, `f2` then `src/parse.rs` accepted whole; alpha stays on the nav as a
/// name-and-branch row with nothing under it, the cursor sits on that row and the right
/// pane reads `nothing pending in alpha`. Never a jump into beta while alpha is listed.
/// (`src/parse.rs` is deliverable 10's addition: alpha has three pending files, and it
/// sorts last, so it is the last file here.)
#[test]
fn tui_accept_last_file_lands_on_the_repo_row() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
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
        app.roots[&alpha].listed(),
        "alpha still has src/parse.rs pending"
    );
    assert_eq!(
        app.selection,
        Some(Selection::Row(
            alpha.clone(),
            fixture_parent::PARSE_RS.as_bytes().to_vec()
        )),
        "the next row in alpha"
    );

    let (_, effect) = app.handle(Action::AcceptFile);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted src/parse.rs");
    assert!(
        !app.roots[&alpha].listed(),
        "alpha has nothing pending any more"
    );
    assert!(
        app.listed_roots().any(|v| v.meta.path == alpha),
        "and is on the nav all the same"
    );
    assert_eq!(
        app.selection,
        Some(Selection::Root(alpha.clone())),
        "the repo's own name row is the last entry above the file that went"
    );
    assert!(engine.scan(&alpha).expect("scan").is_empty());
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("lastcall  3 repos · 3 files"), "{frame}");
    assert!(frame.contains("nothing pending in alpha"), "{frame}");
    snapshot("tui_accept_last_file_lands_on_the_repo_row", &app, W, H);
}

/// §6.7 (Amendment v1.9), deliverable 2: three repos, one of them with nothing pending,
/// and the cursor on it. beta is a dim name-and-branch row with no file rows under it,
/// selectable like any other root row, and the right pane reads `nothing pending in beta`
/// over its branch line. Pressing `t` hides it; the cursor takes the entry now standing at
/// beta's old nav index, and the hint line flips to `t show empty`.
#[test]
fn tui_nav_empty_repo_row() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let beta = root_named(&engine, "beta");
    mark_seen(&mut engine, &beta);
    let mut app = app_of(&mut engine);
    assert!(!app.hide_empty, "§6.1: the default shows every repo");
    app.select(Some(Selection::Root(beta.clone())));
    assert!(app.roots[&beta].rows().is_empty(), "beta has no rows");
    assert!(
        app.listed_roots().any(|v| v.meta.path == beta),
        "and is on the nav"
    );
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("nothing pending in beta"), "{frame}");
    assert!(frame.contains("lastcall  3 repos ·"), "{frame}");
    snapshot("tui_nav_empty_repo_row", &app, W, H);

    assert_eq!(app.handle(Action::HideEmpty).0, Changed::Yes);
    assert!(app.hide_empty);
    assert!(
        !app.listed_roots().any(|v| v.meta.path == beta),
        "`t` hides the repo with nothing pending"
    );
    let (frame, _) = draw(&app, W, H);
    assert!(!frame.contains("beta"), "{frame}");
    // §6.7: the label follows the state, and deliverable 4's drop order keeps the toggle
    // on the 100-column line (the orchestrator's post-checkpoint note: it goes after
    // `^A accept all`, which the header's `[Accept All]` duplicates at this width).
    assert!(frame.contains("t show empty"), "{frame}");
    assert!(!frame.contains("t hide empty"), "{frame}");
    assert!(
        frame.contains("lastcall  2 repos ·"),
        "the header counts the repos on the nav, as it does under a `w` scope: {frame}"
    );
    assert_eq!(
        app.selection,
        Some(Selection::Root(root_named(&engine, "notes"))),
        "the repo left the nav, so the cursor takes the entry at its former index"
    );
    snapshot("tui_hide_empty_toggle", &app, W, H);

    // Independent filters, and the toggle is its own inverse.
    app.handle(Action::HideEmpty);
    assert!(app.listed_roots().any(|v| v.meta.path == beta));
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

/// Phase 8 deliverable 3 / ruling P1: the `$EDITOR` session ended and `f1` on disk is not
/// what the row was rendered from, so the blessing asks before it writes. `y` accepts the
/// bytes the editor left — the row goes, and the ledger holds the *live* oid, not the one
/// the screen was showing when the editor opened.
#[test]
fn tui_editor_return_confirm() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    let mut app = app_of(&mut engine);
    select_row(&mut app, &alpha, "f1");
    let rendered = lastcall_engine::ops::Rendered::of(app.roots[&alpha].row(b"f1").expect("f1"));

    // What the editor did: it wrote the file while lastcall was suspended. The row on
    // screen is still the pre-edit one — no pile has arrived — which is exactly the state
    // the return path has to handle.
    scene
        .repo("alpha")
        .write("f1", "the line the editor left behind\n");
    let live = engine.current(&alpha, b"f1").expect("a watched root");
    let lastcall_engine::store::Current::Present { oid: live_oid, .. } = live.clone() else {
        panic!("f1 is a regular file: {live:?}");
    };
    assert_ne!(
        Some(&live_oid),
        rendered.oid.as_ref(),
        "the editor changed it"
    );

    let (changed, effect) = app.editor_returned(alpha.clone(), rendered, live);
    assert_eq!(changed, Changed::Yes);
    assert_eq!(effect, None, "the question comes first");
    let (frame, _) = draw(&app, W, H);
    assert!(
        frame.contains("f1 edited — mark every hunk in it reviewed?"),
        "{frame}"
    );
    assert!(frame.contains("y / ⏎ confirm    n / Esc cancel"), "{frame}");
    snapshot("tui_editor_return_confirm", &app, W, H);

    // `y`: the accept carries the live oid, so the row is gone and nothing was re-read.
    let (_, effect) = app.handle(Action::Confirm);
    let Some(Effect::Accept(ref reqs)) = effect else {
        panic!("an accept effect, got {effect:?}");
    };
    assert_eq!(reqs.len(), 1);
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "reviewed f1");
    assert!(app.roots[&alpha].row(b"f1").is_none());
    assert!(
        engine
            .scan(&alpha)
            .expect("scan")
            .rows
            .iter()
            .all(|r| r.path != b"f1"),
        "the blessed content is the baseline: nothing pending for f1"
    );
    assert_eq!(
        std::fs::read_to_string(scene.repo("alpha").path().join("f1")).unwrap(),
        "the line the editor left behind\n",
        "the blessing is metadata: the file the editor wrote is untouched"
    );
}

/// G0 Q5: exactly 10 files accept without asking. Only alpha is pending (beta and notes
/// marked seen), with `f1`, `f2`, deliverable 10's `src/parse.rs` and seven generated
/// files; `ctrl-a` folds it at once. The generated count is what keeps the pile at
/// exactly ten — the threshold is the subject here, not alpha's shape.
#[test]
fn tui_accept_all_no_confirm_at_10() {
    let scene = Scene::build();
    let alpha_repo = scene.repo("alpha");
    for i in 1..=7 {
        alpha_repo.write(&format!("g{i:02}"), format!("generated {i}\n"));
    }
    let mut engine = scene.engine();
    let alpha = root_named(&engine, "alpha");
    for name in ["beta", "notes"] {
        let root = root_named(&engine, name);
        mark_seen(&mut engine, &root);
    }
    let mut app = app_of(&mut engine);
    assert_eq!(
        app.roots.values().filter(|v| v.listed()).count(),
        1,
        "only alpha is pending"
    );
    // Amendment v1.9: the other two are on the nav all the same, as empty repo rows.
    assert_eq!(app.listed_roots().count(), 3);
    assert_eq!(app.roots[&alpha].rows().len(), 10);
    let (_, effect) = app.handle(Action::AcceptAll);
    assert!(app.confirm.is_none(), "ten files ask nothing");
    assert!(app.accepting.is_some());
    run_accept(&mut app, &mut engine, effect);
    assert_eq!(status_text(&app), "accepted 10 files in alpha");
    assert!(app.roots.values().all(|v| !v.listed()), "nothing pending");
    assert_eq!(app.listed_roots().count(), 3, "three empty repo rows");
    assert_eq!(
        app.selection, None,
        "the scene never moved the cursor, and the fold does not invent one"
    );
    assert!(engine.scan(&alpha).expect("scan").is_empty());
    let (frame, _) = draw(&app, W, H);
    // Three repos are listed and every one of them is empty, so the pane is the empty
    // state over three empty repo rows — not `select a file` (v1.9; verifier (a) F5).
    assert!(frame.contains("nothing pending across 3 repos"), "{frame}");
    assert!(frame.contains("alpha"), "{frame}");
    snapshot("tui_accept_all_no_confirm_at_10", &app, W, H);
}

/// Ruling 1: an engine capped at 3 rows over alpha with 5 pending files shows the first
/// three by path, `3+ files` in the nav and header, and the notice under the root's
/// main-view header. The five are `f1`, `f2`, deliverable 10's `src/parse.rs` (which
/// sorts last, so it is one of the omitted two) and two generated files — the cap and
/// the omitted count are the subject here, not alpha's shape.
#[test]
fn tui_row_cap_notice() {
    let scene = Scene::build();
    let alpha_repo = scene.repo("alpha");
    for i in 1..=2 {
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
///
/// Since Amendment v1.9 the gate the flag has to beat is `hide_empty`, not the pile — with
/// the toggle off every repo is listed and there is nothing for the flag to rescue — so
/// the scene turns it on.
#[test]
fn tui_herdr_flag_only_root_listed() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let (alpha, beta) = (root_named(&engine, "alpha"), root_named(&engine, "beta"));
    mark_seen(&mut engine, &alpha);
    mark_seen(&mut engine, &beta);
    let mut app = app_of(&mut engine);
    app.hide_empty = true;
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
    // §6.7 (Amendment v1.9): the pane names the repo and keeps the status word; the nav
    // row's own `nothing pending · agent done` line is unchanged behind it.
    assert!(
        frame.contains(&lastcall::tui::render::nothing_pending_in(
            "alpha",
            Some("done")
        )),
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

/// Review (b) F2: the notice is mandatory while the scope is active, and a transient
/// status owns the status row most of the time (one is set at startup, after every accept,
/// on a HEAD change and on a focus verdict, for 30 s each). The two share the row — status
/// left with its age, notice right — instead of the notice disappearing under the status.
#[test]
fn tui_herdr_scope_notice_with_status() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    herdr_connected(&mut app);
    app.handle(Action::Herdr(HerdrUpdate::Scope(Some(Scope {
        label: "alpha".to_owned(),
        roots: [alpha].into_iter().collect(),
    }))));
    app.herdr.scoped = true;
    app.set_status("accepted f1 in alpha");
    let (frame, _) = draw(&app, W, H);
    let row = frame.lines().last().expect("a status row");
    assert!(row.contains("repos hidden"), "{row}");
    assert!(row.contains("accepted f1 in alpha"), "{row}");
    snapshot("tui_herdr_scope_notice_with_status", &app, W, H);
}

// --- Phase 7: restore and flag ----------------------------------------------------------

/// Type into the open note modal one character at a time, as the reader does.
fn type_note(app: &mut App, note: &str) {
    for c in note.chars() {
        app.handle(Action::Note(NoteKey::Edit(EditKey::Insert(c.to_string()))));
    }
}

/// Flag whatever the cursor is on, exactly as the loop does it: `m`, the note a character
/// at a time, Enter, then the engine call the effect asked for and its answer fed back.
/// Nothing here reaches around the reducer — the frames are of an `App` the loop could
/// have produced.
fn flag_here(app: &mut App, engine: &mut Engine, note: &str) {
    assert_eq!(
        app.handle(Action::Flag).0,
        Changed::Yes,
        "the note modal opens"
    );
    type_note(app, note);
    let (_, effect) = app.handle(Action::Note(NoteKey::Send));
    let Some(Effect::Flag {
        root,
        path,
        note,
        hunk,
        summary,
        label,
    }) = effect
    else {
        panic!("a flag effect: {effect:?}");
    };
    let flagged = engine
        .flag(&root, &path, &note, hunk, summary)
        .expect("flag");
    assert!(flagged.outcome.refused.is_empty(), "{:?}", flagged.outcome);
    app.flagged(root, FlagKind::Flag { label }, Ok(flagged));
}

/// One agent pane herdr could stage to.
fn candidate(pane: &str, label: &str, workspace: &str, status: Attention) -> AgentCandidate {
    AgentCandidate {
        pane_id: pane.to_owned(),
        label: label.to_owned(),
        status,
        workspace_label: workspace.to_owned(),
    }
}

/// The note modal over the diff: the **title names the hunk** being flagged, the first line
/// repeats it with the path, the note is as typed (two lines, the caret at the end), and the
/// key line promises `^J` alone — this terminal reports no keyboard enhancement.
#[test]
fn tui_note_modal() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    app.handle(Action::Flag);
    type_note(&mut app, "this rewrite loses the guard\nwhy?");
    assert!(app.note.is_some());
    let (frame, _) = draw(&app, W, H);
    assert!(
        frame.contains("flag hunk 1 of"),
        "the title names it: {frame}"
    );
    assert!(frame.contains("⏎ send   ^J newline"), "{frame}");
    snapshot("tui_note_modal", &app, W, H);
}

/// A note taller than the box scrolls with the caret: eight lines typed, the caret moved up
/// to line 7, and the five visible rows are lines 3 to 7 — the window ends at the caret.
#[test]
fn tui_note_modal_scrolled() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    app.handle(Action::Flag);
    let note: Vec<String> = (1..=8).map(|n| format!("note line {n}")).collect();
    type_note(&mut app, &note.join("\n"));
    // Up once: off line 8 and onto line 7, which is where the window now ends.
    app.handle(Action::Note(NoteKey::Edit(EditKey::Up)));
    assert_eq!(
        app.note.as_ref().expect("open").buf.cursor.line,
        6,
        "the caret is on line 7 (0-based 6)"
    );
    let (frame, _) = draw(&app, W, H);
    for n in 3..=7 {
        assert!(
            frame.contains(&format!("note line {n}")),
            "line {n}: {frame}"
        );
    }
    for n in [1, 2, 8] {
        assert!(
            !frame.contains(&format!("note line {n}")),
            "line {n} is scrolled out: {frame}"
        );
    }
    snapshot("tui_note_modal_scrolled", &app, W, H);
}

/// `m` from the nav has no hunk under a cursor, so the flag is the whole file: the title
/// says `whole file`, the first line says `f1 · whole file`, and the export the send will
/// build carries the row's shape instead of a diff (ruling P4).
#[test]
fn tui_note_modal_whole_file() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Flag);
    type_note(&mut app, "the whole rewrite needs another look");
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("flag whole file"), "the title: {frame}");
    assert!(frame.contains("f1 · whole file"), "the first line: {frame}");
    snapshot("tui_note_modal_whole_file", &app, W, H);
}

/// Two agents under one root: the picker asks which, naming each pane's workspace and
/// status, and says what the flag it is sending was.
#[test]
fn tui_agent_picker() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    let mut candidates = BTreeMap::new();
    candidates.insert(
        alpha.clone(),
        vec![
            candidate("w1:p1", "claude", "lastcall", Attention::Blocked),
            candidate("w2:p3", "codex", "spike", Attention::Working),
        ],
    );
    app.handle(Action::Herdr(HerdrUpdate::Agents(candidates)));
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    flag_here(&mut app, &mut engine, "which of you wrote this?");
    assert!(app.picker.is_some(), "two candidates open the picker");
    app.handle(Action::Pick(PickKey::Down));
    snapshot("tui_agent_picker", &app, W, H);
}

/// `U` on a file with hunks: the confirm modal in its restore form — a different title and
/// a different question from the accept it shares a box with.
#[test]
fn tui_restore_confirm() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    app.handle(Action::RestoreFile);
    assert!(app.confirm.is_some(), "a file restore asks first");
    snapshot("tui_restore_confirm", &app, W, H);
}

/// A flagged hunk keeps its header and gains the note's first line beside it, so the
/// reader sees what they already said here before they say it again.
#[test]
fn tui_diff_view_flagged_hunk() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    flag_here(
        &mut app,
        &mut engine,
        "this loses the guard\nsecond line, not shown",
    );
    let row = app.roots[&alpha].row(b"f1").expect("f1 still pending");
    assert_eq!(row.flags.len(), 1, "{row:?}");
    assert!(row.flags[0].hunk.is_some(), "a hunk flag: {row:?}");
    snapshot("tui_diff_view_flagged_hunk", &app, W, H);
}

/// The nav's own marker: one flag is `⚑`, two on the same file are `⚑2`. The count is the
/// only thing that says a row has more than one note without opening it.
#[test]
fn tui_nav_flag_counts() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");

    // f2 has one hunk: flag the file from the nav, which carries no hunk.
    select_row(&mut app, &alpha, "f2");
    flag_here(&mut app, &mut engine, "is this even needed?");

    // f1: two flags on the one hunk it has, so the row reads `⚑2`.
    select_row(&mut app, &alpha, "f1");
    app.handle(Action::Open);
    flag_here(&mut app, &mut engine, "the guard is gone");
    flag_here(&mut app, &mut engine, "and the test with it");
    let f1 = app.roots[&alpha].row(b"f1").expect("f1 pending");
    assert_eq!(f1.flags.len(), 2, "{f1:?}");
    let f2 = app.roots[&alpha].row(b"f2").expect("f2 pending");
    assert_eq!(f2.flags.len(), 1, "{f2:?}");
    assert!(f2.flags[0].hunk.is_none(), "the nav flags the file: {f2:?}");

    app.select(Some(Selection::Root(alpha.clone())));
    snapshot("tui_nav_flag_counts", &app, W, H);
}

// ---- Phase 8 deliverable 8: the inline editor -------------------------------------------

/// `i` on the selection, answered by the **real** engine — the loop's `Effect::EditInline`
/// round trip with no editor process anywhere near it.
fn open_editor(app: &mut App, engine: &Engine) {
    let (changed, effect) = app.handle(Action::Edit);
    assert_eq!(
        changed,
        Changed::No,
        "nothing is drawn to ask for the bytes"
    );
    let Some(Effect::EditInline(open)) = effect else {
        panic!("an inline-edit effect, got {effect:?}");
    };
    let bytes = engine.read_rendered(&open.root, &open.rendered);
    assert!(bytes.is_ok(), "src/parse.rs opens: {bytes:?}");
    assert_eq!(app.edit_read(open, bytes), (Changed::Yes, None));
    assert!(app.editor.is_some(), "the editor is open");
}

/// Select alpha's `src/parse.rs`, open the diff and put the cursor on the **middle** hunk —
/// the shared start of every editor scene, and the one hunk whose line number proves the
/// editor landed somewhere the fixture chose rather than at the top of the file.
fn at_parse_rs_middle_hunk(app: &mut App, engine: &Engine, alpha: &Path) {
    select_row(app, alpha, fixture_parent::PARSE_RS);
    app.handle(Action::Open);
    app.handle(Action::HunkNext);
    let hunks = app.view_hunks();
    assert_eq!(hunks.len(), 3, "three separated agent hunks");
    assert_eq!(
        hunks[1].editor_line(),
        fixture_parent::parse_rs_edit2_line(),
        "the middle hunk's first changed line, from the fixture text"
    );
    let _ = engine;
}

/// The editor open on the middle hunk: the whole file in a buffer, the caret on the agent's
/// first changed line, the entered hunk tinted as a band, `▎` on the lines of the other two
/// hunks, and a header that names the file and the line.
#[test]
fn tui_editor_open() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    open_editor(&mut app, &engine);

    let ed = app.editor.as_ref().expect("open");
    assert_eq!(ed.line_at_open, fixture_parent::parse_rs_edit2_line());
    assert_eq!(
        ed.buf.text(),
        fixture_parent::PARSE_RS_EDITED,
        "the buffer is the file on disk"
    );
    assert!(ed.band.is_some(), "the entered hunk is tinted: {ed:?}");
    let (frame, _) = draw(&app, W, H);
    assert!(
        frame.contains(&format!(
            "editing src/parse.rs · line {}",
            fixture_parent::parse_rs_edit2_line()
        )),
        "{frame}"
    );
    assert!(frame.contains("^S save   Esc close"), "{frame}");
    snapshot("tui_editor_open", &app, W, H);
}

// ---- deliverable 9: select to copy -------------------------------------------------------

/// Three selected lines of `src/parse.rs`'s middle hunk in reverse video, the rest of the
/// pane untouched: what `v j j` looks like before the `y`.
#[test]
fn tui_diff_selection() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    app.handle(Action::Select);
    app.handle(Action::NavDown);
    app.handle(Action::NavDown);
    let sel = app.sel.expect("a selection");
    assert_eq!(sel.range().1 - sel.range().0, 2, "three lines");
    assert!(app.copy_payload().is_some());
    snapshot("tui_diff_selection", &app, W, H);
}

/// The cue after the copy: a centred box over the diff pane, and the status line still
/// saying what the engine last did.
#[test]
fn tui_copy_cue() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    app.set_status("saved src/parse.rs");
    let (_, effect) = app.handle(Action::Copy);
    let Some(Effect::Copy(bytes)) = effect else {
        panic!("a copy effect, got {effect:?}");
    };
    assert!(
        String::from_utf8_lossy(&bytes).starts_with("@@ -"),
        "the hunk under the cursor, header first"
    );
    assert!(app.cue.is_some());
    let (frame, _) = draw(&app, W, H);
    assert!(frame.contains("copied to clipboard"), "{frame}");
    assert!(frame.contains("saved src/parse.rs"), "{frame}");
    snapshot("tui_copy_cue", &app, W, H);
}

/// The hint line with the diff focused on a frame wide enough for the whole of it:
/// `v select` and `y copy` are the last two hints on it, and the first two off it.
///
/// 142 columns, not the 140 this scene used before deliverable 4: `t hide empty` is on the
/// line now (it used to sit on a tier of its own, above every other, so the widest frame
/// was the only one without it), and the whole line for a file row is fourteen columns
/// longer for it. One column narrower is the same frame without `y copy`.
#[test]
fn tui_hint_diff_focus() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    app.handle(Action::Resize(142, 20));
    let (frame, _) = draw(&app, 142, 20);
    assert!(frame.contains("v select  y copy"), "{frame}");
    let (one_less, _) = draw(&app, 141, 20);
    assert!(one_less.contains("v select"), "{one_less}");
    assert!(!one_less.contains("y copy"), "{one_less}");
    let (narrow, _) = draw(&app, W, H);
    assert!(!narrow.contains("y copy"), "{narrow}");
    snapshot("tui_hint_diff_focus", &app, 142, 20);
}

/// Esc on a buffer that has been typed in asks before throwing the text away, and the
/// question names the file.
#[test]
fn tui_editor_dirty_confirm() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    open_editor(&mut app, &engine);

    app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
        "// typed by the reviewer".to_owned(),
    ))));
    assert!(app.editor.as_ref().expect("open").buf.dirty());
    app.handle(Action::Editor(EditorKey::Close));
    assert_eq!(app.confirm_discard(), Some(&b"src/parse.rs"[..]));
    let (frame, _) = draw(&app, W, H);
    assert!(
        frame.contains("Discard changes to src/parse.rs?"),
        "{frame}"
    );
    snapshot("tui_editor_dirty_confirm", &app, W, H);
}

/// A save the engine refuses because an agent wrote the file while the reader was typing:
/// the buffer is kept whole, the header goes red, and the status names the two keys that
/// reload.
#[test]
fn tui_editor_save_refused() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    open_editor(&mut app, &engine);
    app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
        "// typed by the reviewer\n".to_owned(),
    ))));

    // The agent writes the file underneath the open buffer.
    scene
        .repo("alpha")
        .write(fixture_parent::PARSE_RS, "the agent got there first\n");

    let (_, effect) = app.handle(Action::Editor(EditorKey::Save));
    let Some(Effect::Save {
        root,
        rendered,
        bytes,
    }) = effect
    else {
        panic!("a save effect, got {effect:?}");
    };
    let result = engine
        .save(&root, SaveRequest { rendered, bytes })
        .map_err(|e| AcceptFailed::of(&e));
    let refused = result
        .as_ref()
        .expect("the ledger answered")
        .outcome
        .refused
        .clone();
    assert!(
        matches!(
            refused.first(),
            Some(lastcall_engine::ops::Refused::Moved { .. })
        ),
        "the compare-and-swap saw the agent's write: {refused:?}"
    );
    app.saved(root, fixture_parent::PARSE_RS.as_bytes().to_vec(), result);

    let ed = app.editor.as_ref().expect("the buffer is kept");
    assert!(ed.alarm, "the header is red");
    assert!(
        ed.buf.text().contains("// typed by the reviewer"),
        "every character the reviewer typed is still there"
    );
    let (frame, _) = draw(&app, W, H);
    assert!(
        frame.contains("changed since you opened it; not saved — Esc, then i to reload"),
        "{frame}"
    );
    // Nothing was written: the file on disk is still the agent's.
    let on_disk = std::fs::read_to_string(alpha.join(fixture_parent::PARSE_RS)).expect("read");
    assert_eq!(on_disk, "the agent got there first\n");
    snapshot("tui_editor_save_refused", &app, W, H);
}

/// The editor at 60×20: the gutter and the band survive, and long lines are cut at the
/// right edge with `→` rather than wrapped — an editor that reflows a reader's code while
/// they type in it is lying about the file.
#[test]
fn tui_editor_narrow_60x20() {
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    app.handle(Action::Resize(60, 20));
    open_editor(&mut app, &engine);

    let (frame, _) = draw(&app, 60, 20);
    assert!(frame.contains('→'), "a clipped line says so: {frame}");
    snapshot("tui_editor_narrow_60x20", &app, 60, 20);
}

/// Design pass D9 (ruling R10): the editor header at 60 columns with a path too long for
/// it. Only the **path** gives, and it gives from the head at a `/`, so what is left still
/// reads as a path; `line N/M` — the part that changes as you type — is never the thing
/// cut, and `· unsaved` is reserved at every width so the header does not shift under the
/// reader on the first keystroke. The scene is `tui_editor_narrow_60x20` with the row's
/// path rewritten and one character typed: the body is the same fixture, the header is the
/// whole point.
#[test]
fn tui_editor_long_path_60x20() {
    const LONG: &[u8] = b"crates/lastcall/src/tui/render.rs";
    let scene = Scene::build();
    let mut engine = scene.engine();
    let mut app = app_of(&mut engine);
    let alpha = root_named(&engine, "alpha");
    at_parse_rs_middle_hunk(&mut app, &engine, &alpha);
    app.handle(Action::Resize(60, 20));
    open_editor(&mut app, &engine);
    app.editor.as_mut().expect("open").rendered.path = LONG.to_vec();

    let position = format!("line {}/62", fixture_parent::parse_rs_edit2_line());
    let (clean, _) = draw(&app, 60, 20);
    let header = clean.lines().next().expect("a header").to_owned();
    let head = format!("editing …lastcall/src/tui/render.rs · {position}");
    assert!(
        header.contains(&head),
        "the head gives, cut at a `/`, and the position survives: {header}"
    );
    assert!(!header.contains("unsaved"), "a clean buffer: {header}");

    app.handle(Action::Editor(EditorKey::Edit(EditKey::Insert(
        "x".to_owned(),
    ))));
    let (dirty, _) = draw(&app, 60, 20);
    let dirty_header = dirty.lines().next().expect("a header").to_owned();
    assert!(dirty_header.contains(" · unsaved"), "{dirty_header}");
    assert!(
        dirty_header.contains(&head),
        "the path and the position did not move to make room for it: {dirty_header}"
    );
    snapshot("tui_editor_long_path_60x20", &app, 60, 20);
}

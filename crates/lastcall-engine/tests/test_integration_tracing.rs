//! Phase 6 deliverable 8: the engine's `tracing` probes, read back through a **thread-scoped**
//! subscriber over a real repository.
//!
//! Its own test binary on purpose. `tracing` caches each callsite's `Interest` in a global,
//! and the first thread to reach a callsite decides it for the whole process: inside
//! `crates/lastcall-engine/src/engine.rs`'s unit tests, a sibling scan on another thread —
//! with no subscriber anywhere — re-registers `scan done` as `Interest::never` and this
//! test then sees nothing. Serial, it passed; in parallel it was a coin flip. The fix is a
//! binary where nothing else emits, not `set_global_default` (which would decide the
//! subscriber for every other test in the binary and can be called only once per process).

use std::sync::{Arc, Mutex};

use lastcall_engine::config::Config;
use lastcall_testkit::engine::open_engine;
use lastcall_testkit::fixture_repo::FixtureRepo;
use lastcall_testkit::tmp::TempDir;
use tracing_subscriber::layer::SubscriberExt;

/// Collects each event as `<message> <field>=<value> …`.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);

#[derive(Default)]
struct ProbeLine {
    message: String,
    fields: Vec<String>,
}

impl tracing::field::Visit for ProbeLine {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        let text = text.trim_matches('"').to_owned();
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.push(format!("{}={text}", field.name()));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Probe {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut rendered = ProbeLine::default();
        event.record(&mut rendered);
        let mut line = rendered.message;
        for field in rendered.fields {
            line.push(' ');
            line.push_str(&field);
        }
        self.0.lock().expect("probe lock").push(line);
    }
}

impl Probe {
    fn lines(&self, message: &str) -> Vec<String> {
        self.0
            .lock()
            .expect("probe lock")
            .iter()
            .filter(|l| l.starts_with(message))
            .cloned()
            .collect()
    }

    fn len(&self) -> usize {
        self.0.lock().expect("probe lock").len()
    }
}

fn has_field(line: &str, name: &str) -> bool {
    line.split(' ').any(|f| f.starts_with(&format!("{name}=")))
}

/// The engine's two probes exist, carry the field names the grammar promises, and report
/// real numbers: a scan that found two rows says `rows=2`.
#[test]
fn tracing_engine_open_and_scan_carry_stable_field_names() {
    let mut repo = FixtureRepo::new("eng-trace").expect("fixture repo");
    repo.commit_files(&[("f1", "a\n"), ("f2", "b\n")], "seed")
        .expect("seed commit");
    let state = TempDir::new("lc-trace-state");
    let env = repo.engine_env(state.path());

    let probe = Probe::default();
    let subscriber = tracing_subscriber::registry().with(probe.clone());
    let root = tracing::subscriber::with_default(subscriber, || {
        let mut engine = open_engine(repo.path(), &env, state.path(), Config::default());
        let root = engine.roots()[0].path.clone();
        // First sight, then a real two-file delta so `rows=` is not trivially zero.
        engine.scan(&root).expect("first sight");
        repo.write("f1", "a\nedited\n");
        repo.write("f2", "b\nedited\n");
        assert_eq!(engine.scan(&root).expect("scan").rows.len(), 2);
        root
    });

    let open = probe.lines("open done");
    assert_eq!(open.len(), 1, "one line per open: {open:?}");
    assert!(open[0].contains(" roots=1"), "{}", open[0]);
    assert!(has_field(&open[0], "ms"), "{}", open[0]);

    let scans = probe.lines("scan done");
    assert_eq!(scans.len(), 2, "one line per scan: {scans:?}");
    let last = scans.last().expect("a scan line");
    assert!(last.contains(&format!("root={}", root.display())), "{last}");
    assert!(last.contains(" rows=2"), "{last}");
    assert!(has_field(last, "ms"), "{last}");
    assert!(has_field(last, "seq"), "{last}");

    // Nothing is emitted outside the scope: the engine installs no subscriber of its own,
    // so a `lastcall` that was never asked for logs writes none.
    let before = probe.len();
    let mut engine = open_engine(repo.path(), &env, state.path(), Config::default());
    engine.scan(&root).expect("scan");
    assert_eq!(probe.len(), before);
}

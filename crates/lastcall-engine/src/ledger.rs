//! The seen-state ledger: schema v1.0 (docs/spec/00-spec.md §6.2 verbatim), fail-open
//! loading, atomic writes, locking, and baseline resolution (kickoff deliverable 3).
//!
//! Loading rules (each a unit test):
//! - missing file → [`LoadResult::Missing`] (first sight is the engine's job);
//! - unparsable file or unknown `schema_version` **major** → the file is moved aside to
//!   `ledger.json.unreadable-<unix-secs>-<n>` (never overwritten, never deleted) and the root
//!   opens with `seen_tree = null`, `seen_at.head_commit = null` — every path pending. This
//!   is **not** first sight: first sight at the current HEAD would hide everything committed
//!   since the original first sight;
//! - an override with an unparsable shape → that path resolves to the tree (E2); the raw
//!   value is retained and rewritten so nothing is lost, one notice.
//!
//! `deny_unknown_fields` is deliberately **off**: a newer minor version must still load.
//! Unknown fields are dropped on rewrite.
//!
//! **HEAD is never consulted here** (§6.2): the ledger records what the user has seen as
//! content, and `seen_at.head_commit` is an annotation input, never a baseline input.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::git::{Mode, Oid};
use crate::paths::RepoPaths;
use crate::store::RootKind;

pub const SCHEMA_VERSION: &str = "1.0";

/// Lock retry policy: 40 × 50 ms = 2 s, then the operation errors — never write unlocked.
///
/// Doubled from 20 in Phase 5 (deliverable 2b): workspace scoping makes one lastcall process
/// per pane the normal setup, so a second process holding this root's lock through a
/// `read → merge → write tmp → rename` is routine rather than a collision, and 1 s was thin
/// for that on a cold state dir. Above 2 s an accept stops feeling like a keystroke, so the
/// budget stops there and `LockBusy` becomes the `ledger busy in <root> — try again` line.
pub const LOCK_RETRIES: u32 = 40;
pub const LOCK_BACKOFF: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------------------

/// Injected time source so ledger round-trips are byte-stable in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;

    /// `now()` as ISO-8601 UTC (`2026-01-01T00:00:00Z`).
    fn now_iso8601(&self) -> String {
        iso8601(self.now())
    }
}

/// Production clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock frozen at one instant (tests).
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub SystemTime);

impl FixedClock {
    pub fn at_unix(secs: u64) -> Self {
        Self(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// ISO-8601 UTC with second precision; times before the epoch clamp to it.
pub fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 → y/m/d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------------

/// `seen_at`: where HEAD was when the seen tree was established (annotation input only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenAt {
    pub head_commit: Option<Oid>,
    pub branch: Option<String>,
    pub at: String,
}

/// A flag on a path; never changes the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flag {
    pub note: String,
    pub created_at: String,
}

/// One override. `blob` distinguishes *field absent* (flag-only override: `None`) from
/// `null` (seen as absent: `Some(None)`) from an oid (`Some(Some(oid))`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Override {
    #[serde(
        default,
        deserialize_with = "deserialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub blob: Option<Option<Oid>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub flag: Option<Flag>,
    pub updated_at: String,
}

fn deserialize_double_option<'de, D>(d: D) -> Result<Option<Option<Oid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Oid>::deserialize(d).map(Some)
}

impl Override {
    /// Whether the override still carries anything worth storing.
    pub fn is_empty(&self) -> bool {
        self.blob.is_none() && self.flag.is_none()
    }
}

/// The wire shape: overrides as raw JSON so one unparsable entry cannot sink the file.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerWire {
    schema_version: String,
    root: String,
    kind: RootKind,
    seen_tree: Option<Oid>,
    seen_at: SeenAt,
    #[serde(default)]
    overrides: BTreeMap<String, serde_json::Value>,
}

/// `ledger.json`, one per root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ledger {
    pub schema_version: String,
    /// The canonical root path.
    pub root: String,
    pub kind: RootKind,
    pub seen_tree: Option<Oid>,
    pub seen_at: SeenAt,
    pub overrides: BTreeMap<String, Override>,
    /// Overrides whose shape we could not parse: retained verbatim, rewritten on save, and
    /// ignored by baseline resolution (the path resolves to the tree).
    pub unparsable: BTreeMap<String, serde_json::Value>,
}

impl Ledger {
    /// A fresh ledger (first sight, or the fail-open replacement for an unreadable file).
    pub fn new(root: &Path, kind: RootKind, seen_tree: Option<Oid>, seen_at: SeenAt) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            root: root.to_string_lossy().into_owned(),
            kind,
            seen_tree,
            seen_at,
            overrides: BTreeMap::new(),
            unparsable: BTreeMap::new(),
        }
    }

    /// Overrides that carry a `blob` field (the compaction trigger counts these).
    pub fn blob_override_count(&self) -> usize {
        self.overrides.values().filter(|o| o.blob.is_some()).count()
    }

    fn to_wire(&self) -> LedgerWire {
        let mut overrides: BTreeMap<String, serde_json::Value> = self
            .overrides
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    serde_json::to_value(v).expect("override is serializable"),
                )
            })
            .collect();
        for (k, v) in &self.unparsable {
            overrides.entry(k.clone()).or_insert_with(|| v.clone());
        }
        LedgerWire {
            schema_version: self.schema_version.clone(),
            root: self.root.clone(),
            kind: self.kind,
            seen_tree: self.seen_tree.clone(),
            seen_at: self.seen_at.clone(),
            overrides,
        }
    }

    fn from_wire(wire: LedgerWire, notices: &mut Vec<String>) -> Self {
        let mut overrides = BTreeMap::new();
        let mut unparsable = BTreeMap::new();
        for (path, value) in wire.overrides {
            match serde_json::from_value::<Override>(value.clone()) {
                Ok(o) => {
                    overrides.insert(path, o);
                }
                Err(e) => {
                    notices.push(format!(
                        "override for {path:?} has an unreadable shape ({e}); it resolves to the seen tree"
                    ));
                    unparsable.insert(path, value);
                }
            }
        }
        Self {
            schema_version: wire.schema_version,
            root: wire.root,
            kind: wire.kind,
            seen_tree: wire.seen_tree,
            seen_at: wire.seen_at,
            overrides,
            unparsable,
        }
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(&self.to_wire()).expect("ledger is serializable");
        s.push('\n');
        s
    }
}

// ---------------------------------------------------------------------------------------
// Loading and saving
// ---------------------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not take {} after {retries} tries", path.display())]
    LockBusy { path: PathBuf, retries: u32 },
}

fn io_err(path: &Path, source: std::io::Error) -> LedgerError {
    LedgerError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug)]
pub enum LoadResult {
    /// No ledger file: the engine performs first sight.
    Missing,
    /// The file could not be read as a v1 ledger and was moved aside; open with a null seen
    /// tree (the caller builds that ledger, since it needs root/kind).
    Unreadable { moved_to: PathBuf, reason: String },
    Loaded {
        ledger: Ledger,
        notices: Vec<String>,
    },
}

/// Parse ledger bytes without touching the filesystem.
pub fn parse(bytes: &[u8]) -> Result<(Ledger, Vec<String>), String> {
    let wire: LedgerWire = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let major = wire
        .schema_version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok());
    match major {
        Some(1) => {}
        _ => {
            return Err(format!(
                "unsupported schema_version {:?} (this build reads 1.x)",
                wire.schema_version
            ));
        }
    }
    let mut notices = Vec::new();
    Ok((Ledger::from_wire(wire, &mut notices), notices))
}

/// Load `paths.ledger` per the module rules.
pub fn load(paths: &RepoPaths, clock: &dyn Clock) -> Result<LoadResult, LedgerError> {
    let bytes = match std::fs::read(&paths.ledger) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LoadResult::Missing),
        Err(e) => return Err(io_err(&paths.ledger, e)),
    };
    match parse(&bytes) {
        Ok((ledger, notices)) => Ok(LoadResult::Loaded { ledger, notices }),
        Err(reason) => {
            let moved_to = move_aside(&paths.ledger, clock)?;
            Ok(LoadResult::Unreadable { moved_to, reason })
        }
    }
}

/// Rename `ledger.json` to `ledger.json.unreadable-<secs>-<n>`, never clobbering.
/// Identity of `ledger.json` on disk: (mtime, length, inode). Writes are rename-atomic,
/// so any write yields a new inode; engines compare stamps between scans to notice a
/// ledger written by another process. `None` when the file is absent.
pub type Stamp = (SystemTime, u64, u64);

pub fn stamp(paths: &RepoPaths) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(&paths.ledger).ok()?;
    Some((m.modified().ok()?, m.len(), m.ino()))
}

/// Move the ledger aside (to `ledger.json.unreadable-<secs>-<n>`) without parsing it: the
/// open path's answer to a ledger that belongs to another root.
pub fn move_aside_ledger(paths: &RepoPaths, clock: &dyn Clock) -> Result<PathBuf, LedgerError> {
    move_aside(&paths.ledger, clock)
}

/// The newest `ledger.json.unreadable-*` beside the ledger, if any. With no `ledger.json`
/// next to it, it means another process moved a corrupt ledger aside and has not written
/// its replacement yet (the open path treats that as unreadable, never as first sight).
pub fn moved_aside_sibling(paths: &RepoPaths) -> Option<PathBuf> {
    let dir = paths.ledger.parent()?;
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ledger.json.unreadable-"))
        })
        .collect();
    found.sort();
    found.pop()
}

fn move_aside(ledger: &Path, clock: &dyn Clock) -> Result<PathBuf, LedgerError> {
    let secs = clock
        .now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut n = 0u32;
    loop {
        let candidate = ledger.with_file_name(format!("ledger.json.unreadable-{secs}-{n}"));
        if !candidate.exists() {
            std::fs::rename(ledger, &candidate).map_err(|e| io_err(ledger, e))?;
            return Ok(candidate);
        }
        n += 1;
    }
}

/// Step 1 of a save: write `ledger.json.tmp` in the same directory and `sync_all` it.
pub fn write_tmp(paths: &RepoPaths, ledger: &Ledger) -> Result<PathBuf, LedgerError> {
    let tmp = paths.ledger.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp).map_err(|e| io_err(&tmp, e))?;
    file.write_all(ledger.to_json().as_bytes())
        .map_err(|e| io_err(&tmp, e))?;
    file.sync_all().map_err(|e| io_err(&tmp, e))?;
    Ok(tmp)
}

/// Step 2 of a save: `rename` the temp file over `ledger.json`.
pub fn commit_tmp(paths: &RepoPaths, tmp: &Path) -> Result<(), LedgerError> {
    std::fs::rename(tmp, &paths.ledger).map_err(|e| io_err(tmp, e))
}

/// Atomic save (tmp + fsync + rename). Callers must hold the [`LedgerLock`].
pub fn save(paths: &RepoPaths, ledger: &Ledger) -> Result<(), LedgerError> {
    let tmp = write_tmp(paths, ledger)?;
    commit_tmp(paths, &tmp)
}

/// `<repo>/lock`, held for every read-modify-write of the ledger. Released on drop.
#[derive(Debug)]
pub struct LedgerLock {
    _file: std::fs::File,
}

impl LedgerLock {
    /// Bounded retry (40 × 50 ms = 2 s); after that the operation errors — never write
    /// unlocked. See [`LOCK_RETRIES`] for why the budget is what it is.
    pub fn acquire(paths: &RepoPaths) -> Result<Self, LedgerError> {
        Self::acquire_with(paths, LOCK_RETRIES, LOCK_BACKOFF)
    }

    pub fn acquire_with(
        paths: &RepoPaths,
        retries: u32,
        backoff: Duration,
    ) -> Result<Self, LedgerError> {
        if let Some(parent) = paths.lock.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&paths.lock)
            .map_err(|e| io_err(&paths.lock, e))?;
        for attempt in 0..=retries {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if attempt < retries {
                        std::thread::sleep(backoff);
                    }
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(io_err(&paths.lock, e)),
            }
        }
        Err(LedgerError::LockBusy {
            path: paths.lock.clone(),
            retries,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Baseline resolution (§6.2)
// ---------------------------------------------------------------------------------------

/// What a path's baseline resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Baseline {
    Present {
        oid: Oid,
        mode: Mode,
    },
    /// Seen as absent (an accepted deletion).
    Absent,
    /// Nothing seen: no override, no tree entry (or nothing resolvable).
    Empty,
}

impl Baseline {
    pub fn oid(&self) -> Option<&Oid> {
        match self {
            Baseline::Present { oid, .. } => Some(oid),
            _ => None,
        }
    }
}

/// Object existence, abstracted so the resolution order is unit-testable with a fake store.
pub trait ObjectLookup {
    fn exists_many(&self, oids: &[Oid]) -> Vec<bool>;
}

impl ObjectLookup for crate::store::Store {
    fn exists_many(&self, oids: &[Oid]) -> Vec<bool> {
        crate::store::Store::exists_many(self, oids)
    }
}

impl ObjectLookup for HashSet<Oid> {
    fn exists_many(&self, oids: &[Oid]) -> Vec<bool> {
        oids.iter().map(|o| self.contains(o)).collect()
    }
}

/// The seen tree's entries (`ls-tree -r`), path bytes → (mode, oid).
pub type TreeEntries = BTreeMap<Vec<u8>, (Mode, Oid)>;

/// Resolves baselines for one scan: the existence of every oid the paths can reference is
/// checked in one batch up front (§6.1 verify-on-read).
#[derive(Debug)]
pub struct BaselineResolver<'a> {
    ledger: &'a Ledger,
    tree: &'a TreeEntries,
    present: HashSet<Oid>,
    pub notices: Vec<String>,
}

impl<'a> BaselineResolver<'a> {
    /// `paths` are the candidates this scan will resolve.
    pub fn new(
        ledger: &'a Ledger,
        tree: &'a TreeEntries,
        lookup: &dyn ObjectLookup,
        paths: impl IntoIterator<Item = &'a [u8]>,
    ) -> Self {
        let mut want: Vec<Oid> = Vec::new();
        for p in paths {
            if let Some(o) = std::str::from_utf8(p)
                .ok()
                .and_then(|s| ledger.overrides.get(s))
                && let Some(Some(oid)) = &o.blob
            {
                want.push(oid.clone());
            }
            if let Some((_, oid)) = tree.get(p) {
                want.push(oid.clone());
            }
        }
        want.sort();
        want.dedup();
        let answers = lookup.exists_many(&want);
        let present = want
            .into_iter()
            .zip(answers)
            .filter(|(_, ok)| *ok)
            .map(|(o, _)| o)
            .collect();
        Self {
            ledger,
            tree,
            present,
            notices: Vec::new(),
        }
    }

    /// §6.2 order: override blob → override `null` (Absent) → tree entry → Empty. A
    /// referenced object that does not exist steps to the next rule with a notice.
    pub fn baseline(&mut self, path: &[u8]) -> Baseline {
        if let Some(o) = std::str::from_utf8(path)
            .ok()
            .and_then(|s| self.ledger.overrides.get(s))
        {
            match &o.blob {
                Some(Some(oid)) => {
                    if self.present.contains(oid) {
                        return Baseline::Present {
                            oid: oid.clone(),
                            mode: o.mode.unwrap_or(Mode::Regular),
                        };
                    }
                    self.notices.push(format!(
                        "override blob {oid} for {} is missing from the store; falling back to the seen tree",
                        String::from_utf8_lossy(path)
                    ));
                }
                Some(None) => return Baseline::Absent,
                None => {}
            }
        }
        if let Some((mode, oid)) = self.tree.get(path) {
            if self.present.contains(oid) {
                return Baseline::Present {
                    oid: oid.clone(),
                    mode: *mode,
                };
            }
            self.notices.push(format!(
                "seen-tree blob {oid} for {} is missing from the store (pruned?); treating as unseen",
                String::from_utf8_lossy(path)
            ));
        }
        Baseline::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;

    fn oid(c: char) -> Oid {
        Oid::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn sample() -> Ledger {
        let mut l = Ledger::new(
            Path::new("/canonical/repo"),
            RootKind::Git,
            Some(oid('a')),
            SeenAt {
                head_commit: Some(oid('b')),
                branch: Some("main".into()),
                at: "2026-01-01T00:00:00Z".into(),
            },
        );
        l.overrides.insert(
            "f1".into(),
            Override {
                blob: Some(Some(oid('c'))),
                mode: Some(Mode::Regular),
                flag: None,
                updated_at: "2026-01-01T00:01:00Z".into(),
            },
        );
        l.overrides.insert(
            "gone".into(),
            Override {
                blob: Some(None),
                mode: None,
                flag: None,
                updated_at: "2026-01-01T00:02:00Z".into(),
            },
        );
        l.overrides.insert(
            "flagged".into(),
            Override {
                blob: None,
                mode: None,
                flag: Some(Flag {
                    note: "check this".into(),
                    created_at: "2026-01-01T00:03:00Z".into(),
                }),
                updated_at: "2026-01-01T00:03:00Z".into(),
            },
        );
        l
    }

    #[test]
    fn ledger_json_matches_the_spec_shape_and_round_trips() {
        let l = sample();
        let json = l.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], "1.0");
        assert_eq!(v["kind"], "git");
        assert_eq!(v["seen_tree"], oid('a').as_str());
        assert_eq!(v["seen_at"]["head_commit"], oid('b').as_str());
        assert_eq!(v["overrides"]["f1"]["blob"], oid('c').as_str());
        assert_eq!(v["overrides"]["f1"]["mode"], "100644");
        assert!(v["overrides"]["f1"]["flag"].is_null());
        assert!(v["overrides"]["gone"]["blob"].is_null());
        assert!(
            v["overrides"]["gone"].get("blob").is_some(),
            "null blob is present"
        );
        assert!(
            v["overrides"]["flagged"].get("blob").is_none(),
            "flag-only override has no blob field"
        );
        assert_eq!(v["overrides"]["flagged"]["flag"]["note"], "check this");
        let (back, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty());
        assert_eq!(back, l);
        assert_eq!(back.to_json(), json, "byte-stable");
    }

    #[test]
    fn ledger_unknown_fields_load_and_minor_versions_load() {
        let json = r#"{"schema_version":"1.7","root":"/r","kind":"draft","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x","extra":1},"overrides":{},"future":true}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty());
        assert_eq!(l.kind, RootKind::Draft);
        assert_eq!(l.schema_version, "1.7");
        assert!(
            !l.to_json().contains("future"),
            "unknown fields drop on rewrite"
        );
        assert!(
            parse(
                br#"{"schema_version":"2.0","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn ledger_unparsable_override_is_retained_and_ignored() {
        let json = r#"{"schema_version":"1.0","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},
            "overrides":{"bad":{"blob":42},"ok":{"blob":null,"updated_at":"t"}}}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(l.overrides.contains_key("ok"));
        assert!(!l.overrides.contains_key("bad"));
        assert_eq!(l.unparsable["bad"], serde_json::json!({"blob": 42}));
        let v: serde_json::Value = serde_json::from_str(&l.to_json()).unwrap();
        assert_eq!(v["overrides"]["bad"]["blob"], 42, "rewritten verbatim");
    }

    #[test]
    fn ledger_load_missing_unreadable_and_saved() {
        let dir = TempDir::new("lc-ledger");
        let paths = RepoPaths::under(dir.join("repo"));
        let clock = FixedClock::at_unix(1_767_225_600);
        assert!(matches!(load(&paths, &clock).unwrap(), LoadResult::Missing));
        std::fs::create_dir_all(&paths.repo_dir).unwrap();
        std::fs::write(&paths.ledger, b"{ not json").unwrap();
        let LoadResult::Unreadable { moved_to, .. } = load(&paths, &clock).unwrap() else {
            panic!("expected Unreadable");
        };
        assert_eq!(
            moved_to.file_name().unwrap().to_str().unwrap(),
            "ledger.json.unreadable-1767225600-0"
        );
        assert!(!paths.ledger.exists());
        assert_eq!(std::fs::read(&moved_to).unwrap(), b"{ not json");
        // A second unreadable file in the same second gets the next suffix, not a clobber.
        std::fs::write(&paths.ledger, b"{\"schema_version\":\"9.0\"}").unwrap();
        let LoadResult::Unreadable { moved_to, .. } = load(&paths, &clock).unwrap() else {
            panic!("expected Unreadable");
        };
        assert!(moved_to.ends_with("ledger.json.unreadable-1767225600-1"));
        // Save and load back.
        let l = sample();
        let _lock = LedgerLock::acquire(&paths).unwrap();
        save(&paths, &l).unwrap();
        assert!(!paths.ledger.with_extension("json.tmp").exists());
        let LoadResult::Loaded { ledger, notices } = load(&paths, &clock).unwrap() else {
            panic!("expected Loaded");
        };
        assert!(notices.is_empty());
        assert_eq!(ledger, l);
    }

    #[test]
    fn ledger_lock_is_exclusive_and_bounded() {
        let dir = TempDir::new("lc-lock");
        let paths = RepoPaths::under(dir.join("repo"));
        let held = LedgerLock::acquire(&paths).unwrap();
        let err = LedgerLock::acquire_with(&paths, 2, Duration::from_millis(10)).unwrap_err();
        assert!(
            matches!(err, LedgerError::LockBusy { retries: 2, .. }),
            "{err}"
        );
        // The shipping budget (Phase 5 deliverable 2b): 2 s, not the 1 s of Phase 4.
        assert_eq!(LOCK_RETRIES, 40);
        assert_eq!(LOCK_BACKOFF * LOCK_RETRIES, Duration::from_secs(2));
        drop(held);
        // Bounded retry, not a single attempt: a sibling test may be between fork and
        // exec of a git child at this instant, and the child still shares the flock'd
        // description until its CLOEXEC fds close at exec.
        LedgerLock::acquire(&paths).unwrap();
    }

    #[test]
    fn ledger_baseline_resolution_order_with_a_fake_store() {
        let l = sample();
        let mut tree = TreeEntries::new();
        tree.insert(b"f1".to_vec(), (Mode::Regular, oid('1')));
        tree.insert(b"gone".to_vec(), (Mode::Regular, oid('2')));
        tree.insert(b"flagged".to_vec(), (Mode::Executable, oid('3')));
        tree.insert(b"pruned".to_vec(), (Mode::Regular, oid('4')));
        tree.insert(b"plain".to_vec(), (Mode::Symlink, oid('5')));
        // The fake store lacks 'c' (override blob) and '4' (tree blob).
        let store: HashSet<Oid> = ['1', '2', '3', '5'].into_iter().map(oid).collect();
        let paths: Vec<&[u8]> = vec![b"f1", b"gone", b"flagged", b"pruned", b"plain", b"new"];
        let mut r = BaselineResolver::new(&l, &tree, &store, paths.iter().copied());
        // Override blob missing → tree, with a notice.
        assert_eq!(
            r.baseline(b"f1"),
            Baseline::Present {
                oid: oid('1'),
                mode: Mode::Regular
            }
        );
        assert_eq!(r.notices.len(), 1);
        // Override null → Absent even though the tree has it.
        assert_eq!(r.baseline(b"gone"), Baseline::Absent);
        // Flag-only override → tree entry with the tree's mode.
        assert_eq!(
            r.baseline(b"flagged"),
            Baseline::Present {
                oid: oid('3'),
                mode: Mode::Executable
            }
        );
        // Tree blob missing → Empty with a notice.
        assert_eq!(r.baseline(b"pruned"), Baseline::Empty);
        assert_eq!(r.notices.len(), 2);
        assert_eq!(
            r.baseline(b"plain"),
            Baseline::Present {
                oid: oid('5'),
                mode: Mode::Symlink
            }
        );
        assert_eq!(r.baseline(b"new"), Baseline::Empty);
        // With the override blob present it wins.
        let store: HashSet<Oid> = ['c', '1'].into_iter().map(oid).collect();
        let mut r = BaselineResolver::new(&l, &tree, &store, [&b"f1"[..]]);
        assert_eq!(r.baseline(b"f1").oid(), Some(&oid('c')));
        assert!(r.notices.is_empty());
    }

    #[test]
    fn ledger_iso8601_is_utc_seconds() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            FixedClock::at_unix(1_767_225_600).now_iso8601(),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(
            FixedClock::at_unix(951_782_400 + 3661).now_iso8601(),
            "2000-02-29T01:01:01Z"
        );
    }
}

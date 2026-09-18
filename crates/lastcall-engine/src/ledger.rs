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

/// `1.2` since Amendment v1.12: [`Ledger::seen_branch`] and [`Ledger::branches`] beside
/// the record in force, which stays flattened at the top level. `1.1` (Amendment v1.7)
/// added `Override.flags` beside the 1.0 `flag` mirror. Only the **major** gates
/// readability ([`parse`]), so a 1.0 build still opens a 1.2 file.
pub const SCHEMA_VERSION: &str = "1.2";

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

/// The inverse of [`iso8601`], for the one value a reader has to compare against now: a
/// snooze deadline (Amendment v1.11). Strict about the shape it writes itself —
/// `YYYY-MM-DDTHH:MM:SSZ` — and `None` for anything else, which every caller treats as
/// "not snoozed" rather than as an error.
pub fn parse_iso8601(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|p| p.parse::<i64>().ok());
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// `snoozed_until` as a reader must see it: `Some` only while the deadline is still in the
/// future at `now`. An unparsable value reads as expired, so a hand-edited ledger can never
/// hide a repository forever.
pub fn snooze_active(snoozed_until: Option<&str>, now: SystemTime) -> Option<String> {
    let raw = snoozed_until?;
    let until = parse_iso8601(raw)?;
    (until > now).then(|| raw.to_owned())
}

/// The `YYYY-MM-DD` half of an ISO-8601 instant, for the lines a user reads.
pub fn iso8601_date(s: &str) -> &str {
    s.get(..10).unwrap_or(s)
}

/// Howard Hinnant's `days_from_civil` (y/m/d → days since 1970-01-01).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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

/// The hunk a flag was raised on, captured as it was on screen (Amendment v1.7).
///
/// The rendered text and not a reference to it: a flag outlives the render. By the time the
/// human sends it to the agent the file has usually moved on — that is what they are
/// complaining about — and a stored index into a diff that no longer exists would point at
/// somebody else's lines. `header` is the `@@ -a,b +c,d @@` line and `text` the unified
/// body with its `+`/`-`/space prefixes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagHunk {
    pub index: usize,
    pub header: String,
    pub text: String,
}

/// What a **whole-file** flag was covering when it was raised (Amendment v1.8, additive).
///
/// A hunk flag quotes the lines it objects to, so the export can show them. A whole-file
/// flag has nothing to quote — and "this file" with no shape at all leaves the agent
/// guessing how big "this file" was. These three numbers are the shape as the reviewer saw
/// it on the row, taken at flag time and stored, because a rescan later reads a file the
/// agent may have rewritten (the same reason [`crate::ops::RenderedHunk::of`] travels with
/// the flag rather than coming from the rescan).
///
/// Additive and optional: a flag written before v1.8 has none, and prints no summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagSummary {
    /// Content hunks the row showed (mode-change hunks are not content).
    pub hunks: usize,
    pub added: usize,
    pub deleted: usize,
}

/// A flag on a path; never changes the baseline. `hunk` is `None` for a file flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flag {
    pub note: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunk: Option<FlagHunk>,
    /// The row's shape at flag time, for a whole-file flag only (Amendment v1.8). A hunk
    /// flag never carries one, and the export never prints one for a hunk flag even if a
    /// future writer puts one there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<FlagSummary>,
}

impl Flag {
    /// A file flag: a note with no hunk behind it, and no summary (Phase 7's shape).
    pub fn file(note: impl Into<String>, created_at: impl Into<String>) -> Self {
        Self {
            note: note.into(),
            created_at: created_at.into(),
            hunk: None,
            summary: None,
        }
    }

    /// A whole-file flag that carries the row's shape (Amendment v1.8).
    pub fn whole_file(
        note: impl Into<String>,
        created_at: impl Into<String>,
        summary: FlagSummary,
    ) -> Self {
        Self {
            summary: Some(summary),
            ..Self::file(note, created_at)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Undo (Amendment v1.11, Phase 10 deliverable 2)
// ---------------------------------------------------------------------------------------

/// How deep a root's undo stack goes; the oldest entry is dropped past this.
pub const UNDO_CAP: usize = 20;

/// What one undo entry reverses. The wire spelling is the snake-case name.
///
/// `AcceptDeletion` rather than `AcceptHunk` for a hunk accept on a deletion row: that
/// route delegates to `accept_file` and so to `stage_deletion`, and the record names what
/// was staged, not which key was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoOp {
    AcceptHunk,
    AcceptFile,
    AcceptGroup,
    AcceptDeletion,
    AcceptAll,
    Save,
}

impl UndoOp {
    pub fn as_str(self) -> &'static str {
        match self {
            UndoOp::AcceptHunk => "accept_hunk",
            UndoOp::AcceptFile => "accept_file",
            UndoOp::AcceptGroup => "accept_group",
            UndoOp::AcceptDeletion => "accept_deletion",
            UndoOp::AcceptAll => "accept_all",
            UndoOp::Save => "save",
        }
    }
}

/// One path's baseline as §6.2 resolved it **immediately before** the op.
///
/// `Present` records the oid and the mode; `Absent` and `Empty` both record `null`. The two
/// are not distinguished here on purpose: undo restores through
/// [`crate::ops::Ops::set_override`], whose `equals_tree` rule sends `(None, None)` back to
/// `Absent` on a path the seen tree has and back to `Empty` on a path it does not — which
/// is exactly the distinction, recovered from the tree rather than stored (design review
/// F1, Phase 7's F17 rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoPath {
    pub baseline: Option<Oid>,
    pub mode: Option<Mode>,
}

/// One reversible operation, newest last. Serialised last in the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoEntry {
    pub op: UndoOp,
    pub at: String,
    pub paths: BTreeMap<String, UndoPath>,
}

/// One override. `blob` distinguishes *field absent* (flag-only override: `None`) from
/// `null` (seen as absent: `Some(None)`) from an oid (`Some(Some(oid))`).
///
/// Serialised through [`OverrideWire`], which writes the `flags` list **and** a `flag`
/// mirror of its first entry — see that type for why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub blob: Option<Option<Oid>>,
    pub mode: Option<Mode>,
    /// Every flag on the path, in the order they were raised (v1.7).
    pub flags: Vec<Flag>,
    pub updated_at: String,
}

/// The on-disk shape of an [`Override`].
///
/// **`flag` and `flags` are both written, always.** `flag` is the schema-1.0 mirror of
/// `flags[0]` (without its `hunk` or its `summary`, which 1.0 has no field for), and it is
/// written as `null`
/// rather than omitted when there are no flags. A 1.0 reader drops unknown fields on load,
/// so a 1.0 binary opening a 1.1 file — a merged `main` while this branch is open, or a
/// second machine sharing the state dir — would erase every flag on its first write. With
/// the mirror it keeps the first one. Losing the rest under a downgrade is a §11 residual,
/// not a bug this shape can fix.
///
/// Reading is the mirror image: `flags` when the field is present (even empty), otherwise
/// `flag` lifted into a one-entry list — which is both a genuine 1.0 file and a 1.1 file a
/// 1.0 binary has written back.
#[derive(Debug, Serialize, Deserialize)]
struct OverrideWire {
    #[serde(
        default,
        deserialize_with = "deserialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    blob: Option<Option<Oid>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<Mode>,
    /// The 1.0 mirror. Never `skip_serializing_if`: a 1.0 reader must see the field.
    flag: Option<Flag>,
    #[serde(default)]
    flags: Option<Vec<Flag>>,
    updated_at: String,
}

impl Serialize for Override {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        OverrideWire {
            blob: self.blob.clone(),
            mode: self.mode,
            flag: self.flags.first().map(|f| Flag {
                note: f.note.clone(),
                created_at: f.created_at.clone(),
                hunk: None,
                summary: None,
            }),
            flags: Some(self.flags.clone()),
            updated_at: self.updated_at.clone(),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Override {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = OverrideWire::deserialize(d)?;
        Ok(Override {
            blob: w.blob,
            mode: w.mode,
            flags: w.flags.unwrap_or_else(|| w.flag.into_iter().collect()),
            updated_at: w.updated_at,
        })
    }
}

/// `null` → `Some(None)`, a value → `Some(Some(v))`, and the field being **absent** →
/// `None` (serde's `default`). Two fields need that three-way distinction: an override's
/// `blob` (flag-only, seen-as-absent, or an oid) and the ledger's `seen_branch` (a 1.1
/// file, a record made at a detached HEAD, or a branch name).
fn deserialize_double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

impl Override {
    /// Whether the override still carries anything worth storing.
    pub fn is_empty(&self) -> bool {
        self.blob.is_none() && self.flags.is_empty()
    }

    /// Whether a fold has to leave this override's `blob` where it is (verifier G1).
    ///
    /// A fold folds every override into the seen tree and then clears it, because the tree
    /// now says what the override said. One shape cannot be folded away: R2's release of a
    /// path in a **watched folder** — `blob: null` on a file too large to read — when the
    /// user's note is still on it. The tree cannot hold "seen as absent", so clearing the
    /// blob would leave a flag-only override, which the scan reads as "the record holds
    /// this path" and shows again. The note has to survive every fold (R6) and so does the
    /// accept, so the release stays.
    ///
    /// The caller adds the other half of the test: the new tree does not hold the path.
    /// A repository's record never takes this branch, so its folds are what they were.
    pub fn survives_fold(&self, kind: RootKind) -> bool {
        kind == RootKind::Draft && matches!(self.blob, Some(None)) && !self.flags.is_empty()
    }
}

/// One branch's parked seen record (Amendment v1.12, R3): everything the record in force
/// carries, plus when the branch was left.
///
/// The record **in force** is deliberately *not* one of these — it stays flattened at the
/// top level of the document (R6) so a 1.1 binary opening a 1.2 file still reads the
/// baseline of the branch it is on. `overrides` are parsed strictly here: a parked record
/// whose shape does not load is dropped with a notice (that branch first-sights again at
/// its next checkout, an over-show), which is why [`LedgerWire::branches`] holds raw JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRecord {
    pub seen_tree: Option<Oid>,
    pub seen_at: SeenAt,
    #[serde(default)]
    pub overrides: BTreeMap<String, Override>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undo: Vec<UndoEntry>,
    /// When the branch was last left (ISO-8601 UTC).
    pub parked_at: String,
}

/// The wire shape: overrides as raw JSON so one unparsable entry cannot sink the file.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerWire {
    schema_version: String,
    root: String,
    kind: RootKind,
    seen_tree: Option<Oid>,
    seen_at: SeenAt,
    /// Which branch the top-level record belongs to (Amendment v1.12, R6).
    /// `Option<Option<String>>` so *absent* — a file a 1.1 binary wrote, whose record the
    /// first sync adopts for whatever branch `HEAD` names — stays distinguishable from
    /// `null`, which a 1.2 binary writes for a record made at a detached HEAD or for a
    /// draft root. **No `skip_serializing_if`:** once the document is 1.2 the field is
    /// always there, so the distinction survives every rewrite.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    seen_branch: Option<Option<String>>,
    /// The commit the root was first sighted at (Amendment v1.12, R2's seen-state target).
    /// Omitted when unknown, so a 1.1 file and a 1.2 file written before the field both
    /// read as `None` and rewrite byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_sight_head: Option<Oid>,
    #[serde(default)]
    overrides: BTreeMap<String, serde_json::Value>,
    /// The parked records (Amendment v1.12). Omitted when empty, so a root that has only
    /// ever been on one branch rewrites exactly as it did before this phase; raw JSON for
    /// the same reason `overrides` is.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    branches: BTreeMap<String, serde_json::Value>,
    /// Amendment v1.11, additive. Omitted when the root is not snoozed, so a ledger that
    /// never met a Phase 10 binary is byte-identical after a rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snoozed_until: Option<String>,
    /// Amendment v1.11, additive and serialised last. Omitted when empty, same reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    undo: Vec<UndoEntry>,
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
    /// The branch whose record is in force (Amendment v1.12, R1): the name in
    /// `<git_dir>/HEAD`. `None` for a draft root and for a record made at a detached HEAD.
    pub seen_branch: Option<String>,
    /// The commit this root was first sighted at (R2's seen-state target): set once, at
    /// the root's first sight, and never changed by an accept, a switch, an adoption, a
    /// rename or a compaction. It belongs to the root, not to a branch, so no
    /// [`BranchRecord`] carries it. `None` when the root was first sighted at an unborn
    /// head, when it is a draft root, or when the state file predates the field.
    pub first_sight_head: Option<Oid>,
    pub overrides: BTreeMap<String, Override>,
    /// One parked record per branch this root has been checked out on while lastcall
    /// watched, never including [`Ledger::seen_branch`] (R3).
    pub branches: BTreeMap<String, BranchRecord>,
    /// The file carried no `seen_branch` field at all (a 1.1 binary wrote it), so the
    /// first sync adopts `HEAD`'s branch as this record's owner with no switch and no fold
    /// (R6). In memory only: it is never written, and `false` the moment a sync has run.
    pub adopt_branch: bool,
    /// Overrides whose shape we could not parse: retained verbatim, rewritten on save, and
    /// ignored by baseline resolution (the path resolves to the tree).
    pub unparsable: BTreeMap<String, serde_json::Value>,
    /// When this root stops being hidden from the nav (Amendment v1.11). An expired value
    /// reads as `None` through [`snooze_active`] and is cleared by the next ledger write.
    pub snoozed_until: Option<String>,
    /// The undo stack, oldest first, at most [`UNDO_CAP`] deep.
    pub undo: Vec<UndoEntry>,
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
            seen_branch: None,
            first_sight_head: None,
            overrides: BTreeMap::new(),
            branches: BTreeMap::new(),
            adopt_branch: false,
            unparsable: BTreeMap::new(),
            snoozed_until: None,
            undo: Vec::new(),
        }
    }

    /// Overrides a fold would fold away (the compaction trigger counts these).
    ///
    /// Every override that carries a `blob`, minus the releases a fold has to keep
    /// (verifier G1): counting those would hold the record permanently over the threshold
    /// and make every later accept re-run a compaction that changes nothing.
    pub fn blob_override_count(&self) -> usize {
        self.overrides
            .values()
            .filter(|o| o.blob.is_some() && !o.survives_fold(self.kind))
            .count()
    }

    /// Push one undo entry, dropping the oldest past [`UNDO_CAP`].
    pub fn push_undo(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
        while self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
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
            // **Every write stamps the current version** (Amendment v1.7 §6.2; verifier
            // F8). What `to_wire` produces is a 1.1 document — the `flags` list, the `flag`
            // mirror, `flag` present as `null` rather than omitted — whatever the file we
            // read said. Re-stamping the version we read meant a 1.0 file gained 1.1 fields
            // while still claiming 1.0, and a 1.7 file kept a 1.7 stamp after this build had
            // already dropped every 1.7 field it did not understand. Both are documents
            // whose own version number is a lie about what is in them.
            schema_version: SCHEMA_VERSION.to_string(),
            root: self.root.clone(),
            kind: self.kind,
            seen_tree: self.seen_tree.clone(),
            seen_at: self.seen_at.clone(),
            // Always `Some(..)`, so the field is always written: see `LedgerWire`.
            seen_branch: Some(self.seen_branch.clone()),
            first_sight_head: self.first_sight_head.clone(),
            overrides,
            branches: self
                .branches
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        serde_json::to_value(v).expect("branch record is serializable"),
                    )
                })
                .collect(),
            snoozed_until: self.snoozed_until.clone(),
            undo: self.undo.clone(),
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
        let mut branches = BTreeMap::new();
        for (name, value) in wire.branches {
            match serde_json::from_value::<BranchRecord>(value) {
                Ok(r) => {
                    branches.insert(name, r);
                }
                Err(e) => notices.push(format!(
                    "the parked seen record for branch {name:?} has an unreadable shape ({e}); \
                     that branch starts again at its next checkout"
                )),
            }
        }
        Self {
            schema_version: wire.schema_version,
            root: wire.root,
            kind: wire.kind,
            seen_tree: wire.seen_tree,
            seen_at: wire.seen_at,
            // Absent (a 1.1 file) and `null` (a 1.2 file whose record was made detached)
            // both read as `None` in memory; only the first asks for the adoption.
            adopt_branch: wire.seen_branch.is_none(),
            seen_branch: wire.seen_branch.flatten(),
            first_sight_head: wire.first_sight_head,
            overrides,
            branches,
            unparsable,
            snoozed_until: wire.snoozed_until,
            undo: wire.undo,
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

// `Loaded` is the common case and the ledger is moved out of it immediately; boxing it would
// buy an allocation per load to shrink a value that is never held in a collection.
#[allow(clippy::large_enum_variant)]
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
                flags: Vec::new(),
                updated_at: "2026-01-01T00:01:00Z".into(),
            },
        );
        l.overrides.insert(
            "gone".into(),
            Override {
                blob: Some(None),
                mode: None,
                flags: Vec::new(),
                updated_at: "2026-01-01T00:02:00Z".into(),
            },
        );
        l.overrides.insert(
            "flagged".into(),
            Override {
                blob: None,
                mode: None,
                flags: vec![Flag::file("check this", "2026-01-01T00:03:00Z")],
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
        assert_eq!(v["schema_version"], "1.2");
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

    /// Amendment v1.7: a genuine schema-1.0 file has `flag` and no `flags`; the reader lifts
    /// the one flag into the list, and it is a file flag (no hunk).
    #[test]
    fn ledger_1_0_flag_lifts_into_flags() {
        let json = r#"{"schema_version":"1.0","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},
            "overrides":{"f1":{"flag":{"note":"look","created_at":"t"},"updated_at":"t"},
                         "f2":{"blob":null,"flag":null,"updated_at":"t"}}}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(l.overrides["f1"].flags, vec![Flag::file("look", "t")]);
        assert!(l.overrides["f1"].flags[0].hunk.is_none());
        assert!(
            l.overrides["f2"].flags.is_empty(),
            "a null flag is no flags"
        );
        // The rewrite gains the `flags` list — and says so: what is written is a 1.2
        // document, so it is stamped 1.2 (F8), not the 1.0 it was read as.
        let v: serde_json::Value = serde_json::from_str(&l.to_json()).unwrap();
        assert_eq!(v["schema_version"], "1.2");
        assert_eq!(v["overrides"]["f1"]["flags"][0]["note"], "look");
        assert_eq!(
            v["overrides"]["f1"]["flag"]["note"], "look",
            "and the 1.0 mirror is still there"
        );
    }

    /// A write says what it wrote (Amendment v1.7 §6.2; verifier F8).
    ///
    /// Before this, `to_wire` re-stamped the version it had read. A 1.0 file came back with
    /// the 1.1 `flags` list inside it and `"schema_version":"1.0"` on the outside, so a
    /// reader that trusted the stamp — including a future migration keyed on it — was told
    /// the wrong thing about the bytes it was holding. The same in the other direction: a
    /// 1.7 file kept its 1.7 stamp after this build had already dropped every field it did
    /// not understand.
    #[test]
    fn ledger_write_always_stamps_the_current_schema_version() {
        assert_eq!(SCHEMA_VERSION, "1.2");
        let stamp = |json: &str| -> serde_json::Value {
            let (l, _) = parse(json.as_bytes()).unwrap();
            serde_json::from_str(&l.to_json()).unwrap()
        };

        // Older: a genuine 1.0 file, read and written back.
        let older = stamp(
            r#"{"schema_version":"1.0","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},
            "overrides":{"f1":{"flag":{"note":"look","created_at":"t"},"updated_at":"t"}}}"#,
        );
        assert_eq!(older["schema_version"], "1.2");

        // Newer: a minor version this build does not know. It loads (deliberately), but
        // what we write back is 1.2 and is stamped 1.2.
        let newer = stamp(
            r#"{"schema_version":"1.7","root":"/r","kind":"draft","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},"overrides":{},"future":true}"#,
        );
        assert_eq!(newer["schema_version"], "1.2");
        assert!(newer.get("future").is_none());

        // And a ledger built in memory, never read from disk at all.
        let fresh = Ledger::new(
            Path::new("/r"),
            RootKind::Git,
            None,
            SeenAt {
                head_commit: None,
                branch: None,
                at: "x".into(),
            },
        );
        let v: serde_json::Value = serde_json::from_str(&fresh.to_json()).unwrap();
        assert_eq!(v["schema_version"], "1.2");
    }

    /// The dual write: `flag` mirrors `flags[0]` without its hunk, `flags` carries all of
    /// them with their hunks, and the pair round-trips through the reader unchanged.
    #[test]
    fn ledger_1_1_round_trips_both_fields() {
        let mut l = sample();
        let hunk = FlagHunk {
            index: 1,
            header: "@@ -1,3 +1,3 @@".into(),
            text: "-a\n+b\n c\n".into(),
        };
        l.overrides.insert(
            "many".into(),
            Override {
                blob: None,
                mode: None,
                flags: vec![
                    Flag {
                        note: "first".into(),
                        created_at: "t1".into(),
                        hunk: Some(hunk.clone()),
                        summary: None,
                    },
                    Flag::file("second", "t2"),
                ],
                updated_at: "t2".into(),
            },
        );
        let json = l.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], "1.2");
        let o = &v["overrides"]["many"];
        assert_eq!(o["flag"]["note"], "first", "the 1.0 mirror is flags[0]");
        assert!(
            o["flag"].get("hunk").is_none(),
            "the mirror carries no hunk — 1.0 has no field for it"
        );
        assert_eq!(o["flags"].as_array().unwrap().len(), 2);
        assert_eq!(o["flags"][0]["hunk"]["header"], "@@ -1,3 +1,3 @@");
        assert_eq!(o["flags"][0]["hunk"]["index"], 1);
        assert!(
            o["flags"][1].get("hunk").is_none(),
            "a file flag omits the field"
        );
        let (back, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(back, l, "flags win over the mirror on the way back");
        assert_eq!(back.to_json(), json, "byte-stable");
    }

    /// The post-downgrade shape (F4): a 1.0 binary read this 1.1 file, dropped `flags` as an
    /// unknown field, and wrote it back with `flag` alone. The first flag survives; the rest
    /// are the §11 residual.
    #[test]
    fn ledger_1_1_with_flags_absent_lifts_flag() {
        let json = r#"{"schema_version":"1.1","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},
            "overrides":{"f1":{"flag":{"note":"first","created_at":"t1"},"updated_at":"t2"}}}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(l.overrides["f1"].flags, vec![Flag::file("first", "t1")]);
        // An explicitly empty `flags` is authoritative, not a missing one.
        let cleared = r#"{"schema_version":"1.1","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},
            "overrides":{"f1":{"blob":null,"flag":{"note":"stale","created_at":"t1"},
                               "flags":[],"updated_at":"t2"}}}"#;
        let (l, _) = parse(cleared.as_bytes()).unwrap();
        assert!(l.overrides["f1"].flags.is_empty());
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

    /// Amendment v1.12's wire shape: `seen_branch` at the top level and two parked records
    /// under `branches`, through a full write and read.
    #[test]
    fn ledger_1_2_round_trips_two_parked_records() {
        let mut l = sample();
        l.seen_branch = Some("feat/w".into());
        let rec = |tree: char, at: &str| BranchRecord {
            seen_tree: Some(oid(tree)),
            seen_at: SeenAt {
                head_commit: Some(oid(tree)),
                branch: Some("ignored".into()),
                at: at.into(),
            },
            overrides: BTreeMap::from([(
                "p".to_string(),
                Override {
                    blob: Some(Some(oid('e'))),
                    mode: Some(Mode::Regular),
                    flags: Vec::new(),
                    updated_at: at.into(),
                },
            )]),
            undo: Vec::new(),
            parked_at: at.into(),
        };
        l.branches.insert("main".into(), rec('a', "t1"));
        l.branches.insert("run-1".into(), rec('b', "t2"));

        let json = l.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], "1.2");
        assert_eq!(v["seen_branch"], "feat/w");
        assert_eq!(v["branches"]["main"]["seen_tree"], oid('a').as_str());
        assert_eq!(v["branches"]["run-1"]["parked_at"], "t2");
        assert_eq!(
            v["branches"]["main"]["overrides"]["p"]["blob"],
            oid('e').as_str()
        );
        assert!(
            v["branches"]["main"].get("undo").is_none(),
            "an empty undo stack is omitted inside a parked record too"
        );

        let (back, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(back.seen_branch.as_deref(), Some("feat/w"));
        assert_eq!(back.branches, l.branches);
        assert!(!back.adopt_branch, "a 1.2 file names its branch");
        assert_eq!(back.to_json(), json, "and the second write is identical");
    }

    /// R6's other half: a root that has only ever been on one branch gains exactly one key
    /// over its 1.1 self, and rewrites byte-for-byte from then on — with and without the
    /// first-sight head, which is the other field this schema omits when it has nothing to
    /// say (R2's seen-state target).
    #[test]
    fn ledger_1_2_with_no_parked_records_rewrites_byte_identical() {
        let mut l = sample();
        l.seen_branch = Some("main".into());
        let once = l.to_json();
        assert!(
            !once.contains("\"branches\""),
            "no parked records, no field: {once}"
        );
        assert!(
            !once.contains("\"first_sight_head\""),
            "an unknown first-sight head is omitted, so a file written before the field \
             rewrites unchanged: {once}"
        );
        let (back, notices) = parse(once.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert!(back.branches.is_empty());
        assert!(back.first_sight_head.is_none());
        assert_eq!(back.to_json(), once, "rewrite is byte-identical");

        // And the same for the shape that has it.
        let mut with_head = l.clone();
        with_head.first_sight_head = Some(oid('d'));
        let text = with_head.to_json();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["first_sight_head"], oid('d').as_str());
        let (back, notices) = parse(text.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(back.first_sight_head, Some(oid('d')));
        assert_eq!(back.to_json(), text, "rewrite is byte-identical");

        // And the detached case is `null`, not an absent key: a reader must be able to tell
        // "this record belongs to no branch" from "a 1.1 binary wrote this".
        let mut detached = l.clone();
        detached.seen_branch = None;
        let text = detached.to_json();
        assert!(text.contains("\"seen_branch\": null"), "{text}");
        let (back, _) = parse(text.as_bytes()).unwrap();
        assert!(back.seen_branch.is_none());
        assert!(
            !back.adopt_branch,
            "an explicit null is this build saying the record belongs to no branch, which \
             is not the same thing as a 1.1 binary having no field to say it with"
        );
    }

    /// D21's fixture as a string: the file a 1.1 binary wrote has neither field, loads
    /// clean, and asks to be adopted.
    #[test]
    fn ledger_1_1_file_has_no_branch_and_asks_to_be_adopted() {
        let json = r#"{"schema_version":"1.1","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},
            "overrides":{"f1":{"blob":null,"flags":[],"updated_at":"t"}}}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert!(l.seen_branch.is_none());
        assert!(l.branches.is_empty());
        assert!(l.adopt_branch, "the missing field is the adoption signal");
        assert!(
            l.first_sight_head.is_none(),
            "a file older than the field knows no first-sight head, and R2's clause (a) \
             simply never holds for it"
        );
        assert!(l.overrides.contains_key("f1"), "nothing else moved");
        // The adoption itself is `RootState::sync_branch`'s, and is not written until the
        // next ordinary write; what this build writes is 1.2 and says so.
        let v: serde_json::Value = serde_json::from_str(&l.to_json()).unwrap();
        assert_eq!(v["schema_version"], "1.2");
        assert!(v["seen_branch"].is_null());
    }

    /// R2's seen-state target is additive: a 1.2 file written before `first_sight_head`
    /// existed reads as `None` — the same answer a 1.1 file gives — and no parked record
    /// carries one, because the commit the root was first sighted at belongs to the root
    /// and not to any branch.
    #[test]
    fn ledger_a_1_2_file_without_a_first_sight_head_reads_as_none() {
        let json = r#"{"schema_version":"1.2","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},"seen_branch":"main",
            "overrides":{},
            "branches":{"old":{"seen_tree":null,"seen_at":{"head_commit":null,"branch":null,
                               "at":"x"},"overrides":{},"parked_at":"t"}}}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert!(notices.is_empty(), "{notices:?}");
        assert!(l.first_sight_head.is_none());
        assert_eq!(l.seen_branch.as_deref(), Some("main"));
        let v: serde_json::Value = serde_json::from_str(&l.to_json()).unwrap();
        assert!(
            v.get("first_sight_head").is_none(),
            "and the rewrite does not invent one"
        );
        assert!(
            v["branches"]["old"].get("first_sight_head").is_none(),
            "a parked record never carries it"
        );
    }

    /// A parked record whose shape this build cannot read costs that branch its record,
    /// never the file: the branch starts again at its next checkout, which over-shows.
    #[test]
    fn ledger_an_unparsable_parked_record_is_dropped_with_a_notice() {
        let json = r#"{"schema_version":"1.2","root":"/r","kind":"git","seen_tree":null,
            "seen_at":{"head_commit":null,"branch":null,"at":"x"},"seen_branch":"main",
            "overrides":{},
            "branches":{"bad":{"seen_tree":42},
                        "good":{"seen_tree":null,"seen_at":{"head_commit":null,"branch":null,
                                "at":"x"},"overrides":{},"parked_at":"t"}}}"#;
        let (l, notices) = parse(json.as_bytes()).unwrap();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0]
                .starts_with("the parked seen record for branch \"bad\" has an unreadable shape"),
            "{}",
            notices[0]
        );
        assert!(notices[0].ends_with("that branch starts again at its next checkout"));
        assert!(l.branches.contains_key("good"));
        assert!(!l.branches.contains_key("bad"));
        assert_eq!(l.seen_branch.as_deref(), Some("main"));
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

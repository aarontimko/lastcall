//! The seen-on-another-branch annotation (Phase 14 B, Amendment v1.15, §6.4): a layer
//! after the scan, beside `upstream.rs`, called at the same point and after it.
//!
//! For a git root with at least one parked record (`Ledger::branches`), every row whose
//! current side is present is compared with the composed baseline of its path in each
//! parked record: the record's own override when it carries a blob (`mode: None` read as
//! `Mode::Regular`, as `BaselineResolver::baseline` reads it), else the entry in the
//! record's `seen_tree`. An exact match on the oid and on the mode, through the scan's
//! mode normalisation, puts the branch name on [`Row::seen_on`]. The record in force is
//! never compared (a match there would not be pending), and a parked override that says
//! "seen as absent" (`blob: Some(None)`) or carries only flags never matches: the first
//! has no content to match, the second is a path the user left a note on over there.
//!
//! A row annotated `upstream` or `mixed` is never marked: upstream is the older label and
//! says "someone else's commit", so such a row stays in the upstream group alone.
//!
//! **Cost.** Each parked record's tree is listed once and kept in [`SeenCache`] under
//! `(branch, tree oid)`. A parked tree changes only when a switch parks a record, so every
//! scan after the first is a lookup; the cache drops every key whose branch is no longer
//! parked at that tree (so `prune_parked` shrinks it), overrides are read live from the
//! ledger and never cached, and nothing is listed when no row is a candidate.
//!
//! `scan.rs` stays ignorant of HEAD and of parked records; this module never drops a row
//! and never reorders one. Whether a marked row folds is [`Row::folds_seen`].

use std::collections::HashMap;

use crate::git::{Mode, Oid};
use crate::ledger::{Ledger, TreeEntries};
use crate::scan::Pile;

/// Parked trees listed so far, keyed by `(branch, tree oid)`.
#[derive(Debug, Default)]
pub struct SeenCache {
    trees: HashMap<(String, Oid), TreeEntries>,
}

impl SeenCache {
    /// How many parked trees are held (tests and the engine's debug line).
    pub fn len(&self) -> usize {
        self.trees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trees.is_empty()
    }

    /// Drop every tree whose branch is no longer parked at that tree: a branch
    /// `prune_parked` removed, or one a later switch parked again at another tree.
    fn prune(&mut self, ledger: &Ledger) {
        self.trees.retain(|(branch, tree), _| {
            ledger
                .branches
                .get(branch)
                .and_then(|r| r.seen_tree.as_ref())
                == Some(tree)
        });
    }
}

/// Mark every row of `pile` whose content a parked record already accepted.
///
/// `filemode` is the store's `core.filemode` (the scan's normalisation: with it off, an
/// executable bit is not a difference). `list` lists one tree (`Store::ls_tree` in the
/// engine); it runs at most once per parked `(branch, tree)` for the cache's lifetime.
/// On an error every mark this call made is taken back and the error returned, so the
/// caller's notice describes a pile with no seen marks at all rather than some.
pub fn mark<E>(
    pile: &mut Pile,
    ledger: &Ledger,
    filemode: bool,
    cache: &mut SeenCache,
    mut list: impl FnMut(&Oid) -> Result<TreeEntries, E>,
) -> Result<(), E> {
    cache.prune(ledger);
    for row in &mut pile.rows {
        row.seen_on.clear();
    }
    let norm = |m: Mode| -> Mode {
        if !filemode && m == Mode::Executable {
            Mode::Regular
        } else {
            m
        }
    };
    let candidates: Vec<usize> = pile
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.current.is_some() && r.annotation.is_none())
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    // `branches` is a BTreeMap, so `seen_on` comes out sorted by branch name.
    for (branch, record) in &ledger.branches {
        if ledger.seen_branch.as_ref() == Some(branch) {
            continue;
        }
        let override_of = |path: &[u8]| {
            std::str::from_utf8(path)
                .ok()
                .and_then(|p| record.overrides.get(p))
        };
        // The tree is listed only when some candidate would read it.
        let needs_tree = candidates
            .iter()
            .any(|&i| override_of(&pile.rows[i].path).is_none());
        let key = record.seen_tree.clone().map(|t| (branch.clone(), t));
        if needs_tree
            && let Some(key) = &key
            && !cache.trees.contains_key(key)
        {
            match list(&key.1) {
                Ok(entries) => {
                    cache.trees.insert(key.clone(), entries);
                }
                Err(e) => {
                    for row in &mut pile.rows {
                        row.seen_on.clear();
                    }
                    return Err(e);
                }
            }
        }
        let tree = key.as_ref().and_then(|k| cache.trees.get(k));
        for &i in &candidates {
            let row = &pile.rows[i];
            let Some(cur) = row.current.as_ref() else {
                continue;
            };
            let baseline: Option<(&Oid, Mode)> = match override_of(&row.path) {
                Some(o) => match &o.blob {
                    Some(Some(oid)) => Some((oid, o.mode.unwrap_or(Mode::Regular))),
                    // Seen as absent, or flag-only: never a match.
                    Some(None) | None => None,
                },
                None => tree
                    .and_then(|t| t.get(&row.path))
                    .map(|(mode, oid)| (oid, *mode)),
            };
            let matched =
                baseline.is_some_and(|(oid, mode)| *oid == cur.oid && norm(mode) == norm(cur.mode));
            if matched {
                pile.rows[i].seen_on.push(branch.clone());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::ledger::{BranchRecord, Override, SeenAt};
    use crate::scan::{Annotation, Change, Entry, Row};
    use crate::store::RootKind;

    fn oid(c: char) -> Oid {
        Oid::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn at() -> SeenAt {
        SeenAt {
            head_commit: None,
            branch: None,
            at: "2026-09-26T00:00:00Z".to_owned(),
        }
    }

    fn row(path: &str, cur: Option<(char, Mode)>) -> Row {
        Row {
            path: path.as_bytes().to_vec(),
            change: if cur.is_some() {
                Change::Modified
            } else {
                Change::Deleted
            },
            baseline: Some(Entry {
                oid: oid('0'),
                mode: Mode::Regular,
            }),
            current: cur.map(|(c, mode)| Entry { oid: oid(c), mode }),
            added: 1,
            deleted: 1,
            hunks: Vec::new(),
            annotation: None,
            conflicted: false,
            collapsed: None,
            flags: Vec::new(),
            rename: None,
            seen_on: Vec::new(),
        }
    }

    fn pile(rows: Vec<Row>) -> Pile {
        Pile {
            rows,
            ..Pile::default()
        }
    }

    fn record(tree: char, overrides: &[(&str, Override)]) -> BranchRecord {
        BranchRecord {
            seen_tree: Some(oid(tree)),
            seen_at: at(),
            overrides: overrides
                .iter()
                .map(|(p, o)| ((*p).to_owned(), o.clone()))
                .collect(),
            undo: Vec::new(),
            parked_at: "2026-09-26T00:00:00Z".to_owned(),
        }
    }

    fn blob(b: Option<Option<Oid>>, mode: Option<Mode>) -> Override {
        Override {
            blob: b,
            mode,
            flags: Vec::new(),
            updated_at: "2026-09-26T00:00:00Z".to_owned(),
        }
    }

    fn ledger(on: &str, parked: Vec<(&str, BranchRecord)>) -> Ledger {
        let mut l = Ledger::new(Path::new("/r"), RootKind::Git, Some(oid('1')), at());
        l.seen_branch = Some(on.to_owned());
        l.branches = parked.into_iter().map(|(b, r)| (b.to_owned(), r)).collect();
        l
    }

    /// One tree for [`Lister::new`]: its oid letter and its `(path, blob letter, mode)`s.
    type TreeSpec<'a> = (char, &'a [(&'a str, char, Mode)]);

    /// The trees the lister knows: tree `t` → entries; every call is counted.
    struct Lister {
        trees: BTreeMap<Oid, TreeEntries>,
        calls: Cell<usize>,
    }

    impl Lister {
        fn new(trees: &[TreeSpec<'_>]) -> Self {
            Self {
                trees: trees
                    .iter()
                    .map(|(t, es)| {
                        (
                            oid(*t),
                            es.iter()
                                .map(|(p, c, m)| (p.as_bytes().to_vec(), (*m, oid(*c))))
                                .collect(),
                        )
                    })
                    .collect(),
                calls: Cell::new(0),
            }
        }

        fn list(&self, t: &Oid) -> Result<TreeEntries, String> {
            self.calls.set(self.calls.get() + 1);
            self.trees
                .get(t)
                .cloned()
                .ok_or_else(|| format!("no tree {t:?}"))
        }
    }

    fn marks(p: &Pile) -> Vec<(String, Vec<String>)> {
        p.rows
            .iter()
            .map(|r| (r.path_lossy(), r.seen_on.clone()))
            .collect()
    }

    fn run(p: &mut Pile, l: &Ledger, lister: &Lister, cache: &mut SeenCache) {
        mark(p, l, true, cache, |t| lister.list(t)).unwrap();
    }

    #[test]
    fn seen_exact_oid_and_mode_marks_the_row() {
        let lister = Lister::new(&[('a', &[("a.rs", 'c', Mode::Regular)])]);
        let l = ledger("feat", vec![("run-1", record('a', &[]))]);
        let mut p = pile(vec![row("a.rs", Some(('c', Mode::Regular)))]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert_eq!(marks(&p), vec![("a.rs".into(), vec!["run-1".into()])]);
    }

    #[test]
    fn seen_a_mode_flip_or_another_oid_is_no_match() {
        let lister = Lister::new(&[(
            'a',
            &[("a.rs", 'c', Mode::Regular), ("b.rs", 'd', Mode::Regular)],
        )]);
        let l = ledger("feat", vec![("run-1", record('a', &[]))]);
        let mut p = pile(vec![
            row("a.rs", Some(('c', Mode::Executable))),
            row("b.rs", Some(('e', Mode::Regular))),
        ]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert!(
            p.rows.iter().all(|r| r.seen_on.is_empty()),
            "{:?}",
            marks(&p)
        );
        // With core.filemode off the executable bit is not a difference, as in the scan.
        mark(&mut p, &l, false, &mut SeenCache::default(), |t| {
            lister.list(t)
        })
        .unwrap();
        assert_eq!(p.rows[0].seen_on, vec!["run-1".to_owned()]);
        assert!(p.rows[1].seen_on.is_empty());
    }

    #[test]
    fn seen_a_parked_override_blob_wins_over_its_tree_entry() {
        // The tree says `x`, the parked record's accept says `y`: `y` is what was seen.
        let lister = Lister::new(&[(
            'a',
            &[("a.rs", 'c', Mode::Regular), ("b.rs", 'c', Mode::Regular)],
        )]);
        let l = ledger(
            "feat",
            vec![(
                "run-1",
                record(
                    'a',
                    &[
                        ("a.rs", blob(Some(Some(oid('d'))), None)),
                        ("b.rs", blob(Some(Some(oid('d'))), Some(Mode::Regular))),
                    ],
                ),
            )],
        );
        let mut p = pile(vec![
            row("a.rs", Some(('d', Mode::Regular))),
            row("b.rs", Some(('c', Mode::Regular))),
        ]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert_eq!(
            marks(&p),
            vec![
                ("a.rs".into(), vec!["run-1".into()]),
                ("b.rs".into(), vec![]),
            ]
        );
    }

    #[test]
    fn seen_an_absent_or_flag_only_override_never_matches() {
        let lister = Lister::new(&[(
            'a',
            &[("a.rs", 'c', Mode::Regular), ("b.rs", 'c', Mode::Regular)],
        )]);
        let l = ledger(
            "feat",
            vec![(
                "run-1",
                record(
                    'a',
                    &[("a.rs", blob(Some(None), None)), ("b.rs", blob(None, None))],
                ),
            )],
        );
        let mut p = pile(vec![
            row("a.rs", Some(('c', Mode::Regular))),
            row("b.rs", Some(('c', Mode::Regular))),
        ]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert!(
            p.rows.iter().all(|r| r.seen_on.is_empty()),
            "{:?}",
            marks(&p)
        );
    }

    #[test]
    fn seen_the_record_in_force_is_never_compared() {
        let lister = Lister::new(&[('a', &[("a.rs", 'c', Mode::Regular)])]);
        // A record under the in-force branch's own name cannot be a parked one; if a
        // ledger ever holds one it is skipped.
        let l = ledger("run-1", vec![("run-1", record('a', &[]))]);
        let mut p = pile(vec![row("a.rs", Some(('c', Mode::Regular)))]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert!(p.rows[0].seen_on.is_empty());
        assert_eq!(lister.calls.get(), 0);
    }

    #[test]
    fn seen_a_path_absent_from_every_parked_record_and_a_deletion_are_no_match() {
        let lister = Lister::new(&[('a', &[("a.rs", 'c', Mode::Regular)])]);
        let l = ledger("feat", vec![("run-1", record('a', &[]))]);
        let mut p = pile(vec![
            row("gone.rs", None),
            row("new.rs", Some(('c', Mode::Regular))),
        ]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert!(
            p.rows.iter().all(|r| r.seen_on.is_empty()),
            "{:?}",
            marks(&p)
        );
    }

    #[test]
    fn seen_two_parked_records_both_matching_list_both_names_sorted() {
        let lister = Lister::new(&[
            ('a', &[("a.rs", 'c', Mode::Regular)]),
            ('b', &[("a.rs", 'c', Mode::Regular)]),
        ]);
        let l = ledger(
            "feat",
            vec![("run-2", record('b', &[])), ("run-1", record('a', &[]))],
        );
        let mut p = pile(vec![row("a.rs", Some(('c', Mode::Regular)))]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert_eq!(
            p.rows[0].seen_on,
            vec!["run-1".to_owned(), "run-2".to_owned()]
        );
    }

    #[test]
    fn seen_an_upstream_or_mixed_row_is_never_marked() {
        let lister = Lister::new(&[(
            'a',
            &[("a.rs", 'c', Mode::Regular), ("b.rs", 'c', Mode::Regular)],
        )]);
        let l = ledger("feat", vec![("run-1", record('a', &[]))]);
        let mut up = row("a.rs", Some(('c', Mode::Regular)));
        up.annotation = Some(Annotation::Upstream);
        let mut mixed = row("b.rs", Some(('c', Mode::Regular)));
        mixed.annotation = Some(Annotation::Mixed);
        let mut p = pile(vec![up, mixed]);
        run(&mut p, &l, &lister, &mut SeenCache::default());
        assert!(
            p.rows.iter().all(|r| r.seen_on.is_empty()),
            "{:?}",
            marks(&p)
        );
        assert_eq!(lister.calls.get(), 0, "no candidate, no listing");
    }

    #[test]
    fn seen_the_cache_lists_a_parked_tree_once_and_a_switch_invalidates_it() {
        let lister = Lister::new(&[
            ('a', &[("a.rs", 'c', Mode::Regular)]),
            ('b', &[("a.rs", 'd', Mode::Regular)]),
        ]);
        let mut cache = SeenCache::default();
        let mut l = ledger("feat", vec![("run-1", record('a', &[]))]);
        let fresh = || pile(vec![row("a.rs", Some(('c', Mode::Regular)))]);
        let mut p = fresh();
        run(&mut p, &l, &lister, &mut cache);
        let mut p = fresh();
        run(&mut p, &l, &lister, &mut cache);
        assert_eq!(p.rows[0].seen_on, vec!["run-1".to_owned()]);
        assert_eq!(lister.calls.get(), 1, "two scans, one listing");
        assert_eq!(cache.len(), 1);

        // An empty pile lists nothing, and neither does a pile of deletions.
        let mut empty = pile(Vec::new());
        run(&mut empty, &l, &lister, &mut SeenCache::default());
        let mut deleted = pile(vec![row("a.rs", None)]);
        run(&mut deleted, &l, &lister, &mut SeenCache::default());
        assert_eq!(lister.calls.get(), 1);

        // A switch parks run-1 again at another tree: the old key goes, the new tree is
        // listed once, and the row no longer matches.
        l.branches.insert("run-1".to_owned(), record('b', &[]));
        let mut p = fresh();
        run(&mut p, &l, &lister, &mut cache);
        assert!(p.rows[0].seen_on.is_empty());
        assert_eq!(lister.calls.get(), 2);
        assert_eq!(cache.len(), 1);

        // Overrides are read live: an accept on the parked record needs no listing.
        l.branches
            .get_mut("run-1")
            .unwrap()
            .overrides
            .insert("a.rs".to_owned(), blob(Some(Some(oid('c'))), None));
        let mut p = fresh();
        run(&mut p, &l, &lister, &mut cache);
        assert_eq!(p.rows[0].seen_on, vec!["run-1".to_owned()]);
        assert_eq!(lister.calls.get(), 2);

        // A pruned branch leaves the cache.
        l.branches.clear();
        let mut p = fresh();
        run(&mut p, &l, &lister, &mut cache);
        assert!(p.rows[0].seen_on.is_empty());
        assert!(cache.is_empty());
    }

    #[test]
    fn seen_a_listing_error_takes_back_every_mark() {
        let lister = Lister::new(&[('a', &[("a.rs", 'c', Mode::Regular)])]);
        let l = ledger(
            "feat",
            vec![("run-1", record('a', &[])), ("run-2", record('f', &[]))],
        );
        let mut p = pile(vec![row("a.rs", Some(('c', Mode::Regular)))]);
        let err = mark(&mut p, &l, true, &mut SeenCache::default(), |t| {
            lister.list(t)
        });
        assert!(err.is_err());
        assert!(p.rows[0].seen_on.is_empty());
    }

    #[test]
    fn seen_the_fold_rule_is_marked_plain_and_unflagged() {
        let mut p = pile(vec![
            row("a.rs", Some(('c', Mode::Regular))),
            row("b.rs", Some(('c', Mode::Regular))),
            row("c.rs", Some(('c', Mode::Regular))),
            row("d.rs", Some(('c', Mode::Regular))),
        ]);
        for r in &mut p.rows {
            r.seen_on = vec!["run-1".to_owned()];
        }
        p.rows[1]
            .flags
            .push(crate::ledger::Flag::file("look", "2026-09-26T00:00:00Z"));
        p.rows[2].annotation = Some(Annotation::Upstream);
        p.rows[3].seen_on.clear();
        let groups = p.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].kind, crate::scan::GroupKind::Upstream);
        assert_eq!(groups[0].paths, vec![b"c.rs".to_vec()]);
        assert_eq!(groups[1].kind, crate::scan::GroupKind::Seen);
        assert_eq!(groups[1].paths, vec![b"a.rs".to_vec()]);
        assert_eq!(p.seen_group().unwrap().paths, vec![b"a.rs".to_vec()]);
    }
}

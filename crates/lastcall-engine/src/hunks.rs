//! The diff model (kickoff deliverable 6): line diffs of two byte buffers, unified hunks at
//! **context 3** so hunk boundaries match `git diff`, never requiring UTF-8.
//!
//! `similar` is used through its slice API over our own `\n`-split lines (its `[u8]` text
//! support needs the `bytes` feature and treats a lone `\r` as a terminator; git does not).
//! [`apply_hunks`] splices the selected hunks into the baseline in one pass: hunk ranges are
//! relative to the original baseline, so applying one at a time would leave the next hunk's
//! offsets stale.

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices, group_diff_ops};

/// Unified-diff context lines (git's default).
pub const CONTEXT: usize = 3;

/// The cap on an on-demand expansion of a collapsed row (§10 2026-09-05 ruling 1): hunk
/// **body lines**, context included, hunk separators excluded. A lockfile rewrite is
/// tens of thousands of lines; the reader wants the shape, not the whole file, and the
/// expansion is held in the UI's memory rather than in the pile.
pub const EXPAND_LINE_CAP: usize = 2_000;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Tag {
    Context,
    Delete,
    Insert,
}

/// One unified hunk. `old_range`/`new_range` are line indexes into the two buffers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hunk {
    pub index: usize,
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    /// Lines with their terminators (the last line of a buffer may lack `\n`).
    pub lines: Vec<(Tag, Vec<u8>)>,
}

impl Hunk {
    /// The synthetic hunk for a mode-only change (D1): `mode 100644 → 100755`.
    pub fn mode_change(old: &str, new: &str) -> Self {
        Self {
            index: 0,
            old_range: 0..0,
            new_range: 0..0,
            lines: vec![
                (Tag::Delete, format!("mode {old}").into_bytes()),
                (Tag::Insert, format!("mode {new}").into_bytes()),
            ],
        }
    }

    /// Whether this is the synthetic D1 mode hunk (empty ranges, `mode …` lines).
    pub fn is_mode_change(&self) -> bool {
        self.old_range.is_empty()
            && self.new_range.is_empty()
            && self.lines.len() == 2
            && self.lines.iter().all(|(_, l)| l.starts_with(b"mode "))
    }

    /// (added, deleted) for this hunk.
    pub fn counts(&self) -> (usize, usize) {
        let added = self.lines.iter().filter(|(t, _)| *t == Tag::Insert).count();
        let deleted = self.lines.iter().filter(|(t, _)| *t == Tag::Delete).count();
        (added, deleted)
    }

    /// The change lines only (context excluded), for the property tests.
    pub fn change_lines(&self) -> Vec<(Tag, Vec<u8>)> {
        self.lines
            .iter()
            .filter(|(t, _)| *t != Tag::Context)
            .cloned()
            .collect()
    }
}

/// Split into lines keeping the `\n` terminators; a final unterminated line is its own line.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

fn ops(old: &[&[u8]], new: &[&[u8]]) -> Vec<DiffOp> {
    capture_diff_slices(Algorithm::Myers, old, new)
}

/// Unified hunks at context 3.
pub fn diff(old: &[u8], new: &[u8]) -> Vec<Hunk> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = ops(&old_lines, &new_lines);
    if ops.iter().all(|op| matches!(op, DiffOp::Equal { .. })) {
        return Vec::new();
    }
    group_diff_ops(ops, CONTEXT)
        .into_iter()
        .enumerate()
        .map(|(index, group)| {
            let mut lines = Vec::new();
            // Ops within a group are contiguous and ordered.
            let (_, first_old, first_new) = group.first().expect("non-empty group").as_tag_tuple();
            let (_, last_old, last_new) = group.last().expect("non-empty group").as_tag_tuple();
            let old_range = first_old.start..last_old.end;
            let new_range = first_new.start..last_new.end;
            for op in &group {
                match *op {
                    DiffOp::Equal { old_index, len, .. } => {
                        for l in &old_lines[old_index..old_index + len] {
                            lines.push((Tag::Context, l.to_vec()));
                        }
                    }
                    DiffOp::Delete {
                        old_index, old_len, ..
                    } => {
                        for l in &old_lines[old_index..old_index + old_len] {
                            lines.push((Tag::Delete, l.to_vec()));
                        }
                    }
                    DiffOp::Insert {
                        new_index, new_len, ..
                    } => {
                        for l in &new_lines[new_index..new_index + new_len] {
                            lines.push((Tag::Insert, l.to_vec()));
                        }
                    }
                    DiffOp::Replace {
                        old_index,
                        old_len,
                        new_index,
                        new_len,
                    } => {
                        for l in &old_lines[old_index..old_index + old_len] {
                            lines.push((Tag::Delete, l.to_vec()));
                        }
                        for l in &new_lines[new_index..new_index + new_len] {
                            lines.push((Tag::Insert, l.to_vec()));
                        }
                    }
                }
            }
            Hunk {
                index,
                old_range,
                new_range,
                lines,
            }
        })
        .collect()
}

/// A collapsed row's hunks, computed on demand and truncated at [`EXPAND_LINE_CAP`]
/// (Phase 6 deliverable 4). Never stored on a [`crate::scan::Row`]: accept stays whole-row
/// for a collapsed path (§6.3 "single accept"), so these hunks are a view, not a baseline.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Expanded {
    pub hunks: Vec<Hunk>,
    /// Body lines the cap dropped; 0 when the whole diff fits.
    pub omitted_lines: usize,
}

/// Diff `old` against `new` and truncate at [`EXPAND_LINE_CAP`].
pub fn expand(old: &[u8], new: &[u8]) -> Expanded {
    truncate(diff(old, new), EXPAND_LINE_CAP)
}

/// Keep whole hunks while the body-line budget lasts, then keep the prefix of the hunk
/// that overruns it (so one enormous hunk still shows something), reporting how many body
/// lines were dropped. The truncated hunk's ranges are narrowed to the lines it kept, so a
/// rendered `@@` header never claims lines that are not on screen. `index` is preserved:
/// nothing accepts through an expansion, and a stable index keeps the view addressable.
pub fn truncate(hunks: Vec<Hunk>, cap: usize) -> Expanded {
    let total: usize = hunks.iter().map(|h| h.lines.len()).sum();
    if total <= cap {
        return Expanded {
            hunks,
            omitted_lines: 0,
        };
    }
    let mut kept: Vec<Hunk> = Vec::new();
    let mut used = 0usize;
    for hunk in hunks {
        if used == cap {
            break;
        }
        let room = cap - used;
        if hunk.lines.len() <= room {
            used += hunk.lines.len();
            kept.push(hunk);
            continue;
        }
        let mut part = hunk;
        part.lines.truncate(room);
        let old_kept = part.lines.iter().filter(|(t, _)| *t != Tag::Insert).count();
        let new_kept = part.lines.iter().filter(|(t, _)| *t != Tag::Delete).count();
        part.old_range = part.old_range.start..part.old_range.start + old_kept;
        part.new_range = part.new_range.start..part.new_range.start + new_kept;
        used = cap;
        kept.push(part);
    }
    Expanded {
        hunks: kept,
        omitted_lines: total - used,
    }
}

/// (added, deleted) line counts, equal to `git diff --numstat`.
pub fn counts(old: &[u8], new: &[u8]) -> (usize, usize) {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let mut added = 0;
    let mut deleted = 0;
    for op in ops(&old_lines, &new_lines) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_len, .. } => deleted += old_len,
            DiffOp::Insert { new_len, .. } => added += new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                deleted += old_len;
                added += new_len;
            }
        }
    }
    (added, deleted)
}

/// Splice exactly the selected hunks (by `index`) into `baseline`, in one pass over the
/// original line numbering. Unselected hunks leave their baseline lines untouched.
pub fn apply_hunks(baseline: &[u8], hunks: &[Hunk], selected: &[usize]) -> Vec<u8> {
    let old_lines = split_lines(baseline);
    let mut chosen: Vec<&Hunk> = hunks
        .iter()
        .filter(|h| selected.contains(&h.index))
        .collect();
    chosen.sort_by_key(|h| h.old_range.start);
    let mut out = Vec::with_capacity(baseline.len());
    let mut cursor = 0usize;
    for h in chosen {
        let start = h.old_range.start.max(cursor);
        for l in &old_lines[cursor..start] {
            out.extend_from_slice(l);
        }
        for (tag, bytes) in &h.lines {
            if *tag != Tag::Delete {
                out.extend_from_slice(bytes);
            }
        }
        cursor = h.old_range.end.max(start);
    }
    for l in &old_lines[cursor..] {
        out.extend_from_slice(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn s(t: &str) -> Vec<u8> {
        t.as_bytes().to_vec()
    }

    /// `n` lines named `<prefix>i\n`, joined.
    fn lines_of(prefix: &str, range: std::ops::Range<usize>) -> Vec<u8> {
        range
            .map(|i| format!("{prefix}{i}\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn hunks_expand_under_the_cap_omits_nothing() {
        let old = lines_of("line ", 0..50);
        let mut new_lines: Vec<String> = (0..50).map(|i| format!("line {i}\n")).collect();
        new_lines[10] = "line 10 edited\n".to_owned();
        new_lines[40] = "line 40 edited\n".to_owned();
        let new = new_lines.concat().into_bytes();
        let expanded = expand(&old, &new);
        assert_eq!(expanded.hunks, diff(&old, &new), "nothing was truncated");
        assert_eq!(expanded.hunks.len(), 2);
        assert_eq!(expanded.omitted_lines, 0);
    }

    #[test]
    fn hunks_truncate_keeps_whole_hunks_while_the_budget_lasts() {
        // Three hunks of 10 body lines each, capped at 25: two whole, then five lines of
        // the third; five of its lines and none of a fourth are the omission.
        let hunks: Vec<Hunk> = (0..3)
            .map(|index| Hunk {
                index,
                old_range: index * 100..index * 100 + 10,
                new_range: index * 100..index * 100 + 10,
                lines: (0..10)
                    .map(|l| {
                        let tag = if l < 4 { Tag::Context } else { Tag::Insert };
                        (tag, format!("h{index} l{l}\n").into_bytes())
                    })
                    .collect(),
            })
            .collect();
        let out = truncate(hunks.clone(), 25);
        assert_eq!(out.hunks.len(), 3);
        assert_eq!(out.hunks[0], hunks[0], "whole");
        assert_eq!(out.hunks[1], hunks[1], "whole");
        assert_eq!(out.hunks[2].lines.len(), 5, "the prefix of the third");
        assert_eq!(out.hunks[2].index, 2, "indexes are preserved");
        // Four context + one insert kept: the old side shows 4 lines, the new side 5.
        assert_eq!(out.hunks[2].old_range, 200..204);
        assert_eq!(out.hunks[2].new_range, 200..205);
        assert_eq!(out.omitted_lines, 5);

        // Exactly at the cap: nothing is truncated and nothing is reported omitted.
        let exact = truncate(hunks.clone(), 30);
        assert_eq!(exact.hunks, hunks);
        assert_eq!(exact.omitted_lines, 0);
    }

    #[test]
    fn hunks_expand_caps_one_enormous_hunk_at_the_line_cap() {
        // A lockfile rewrite: every line changes, so the whole file is one hunk.
        let old = lines_of("old ", 0..5_000);
        let new = lines_of("new ", 0..5_000);
        let full = diff(&old, &new);
        assert_eq!(full.len(), 1, "one contiguous replace");
        let body: usize = full.iter().map(|h| h.lines.len()).sum();
        assert_eq!(body, 10_000, "5,000 deletes + 5,000 inserts");

        let expanded = expand(&old, &new);
        let shown: usize = expanded.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(
            shown, EXPAND_LINE_CAP,
            "the reader sees the cap, not nothing"
        );
        assert_eq!(expanded.omitted_lines, body - EXPAND_LINE_CAP);
        // The narrowed header never claims lines that are not on screen.
        let h = &expanded.hunks[0];
        assert_eq!(h.old_range.len() + h.new_range.len(), EXPAND_LINE_CAP);
    }

    #[test]
    fn hunks_split_lines_keeps_terminators_and_final_partial_line() {
        assert_eq!(split_lines(b"a\nb\n"), vec![&b"a\n"[..], &b"b\n"[..]]);
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
        assert!(split_lines(b"").is_empty());
        assert_eq!(
            split_lines(b"\r\n"),
            vec![&b"\r\n"[..]],
            "a lone CR is not a line end"
        );
    }

    #[test]
    fn hunks_two_separated_edits_make_two_hunks_at_context_three() {
        let old = s("a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\na11\na12\n");
        let new = s("A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\na11\nA12\n");
        let hunks = diff(&old, &new);
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!(hunks[0].old_range, 0..4);
        assert_eq!(hunks[0].counts(), (1, 1));
        assert_eq!(hunks[1].old_range, 8..12);
        assert_eq!(counts(&old, &new), (2, 2));
        assert_eq!(
            apply_hunks(&old, &hunks, &[0]),
            s("A1\na2\na3\na4\na5\na6\na7\na8\na9\na10\na11\na12\n")
        );
        assert_eq!(
            apply_hunks(&old, &hunks, &[1]),
            s("a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\na11\nA12\n")
        );
        assert_eq!(apply_hunks(&old, &hunks, &[0, 1]), new);
        assert_eq!(apply_hunks(&old, &hunks, &[]), old);
        assert!(diff(&old, &old).is_empty());
    }

    #[test]
    fn hunks_empty_sides_and_missing_trailing_newline() {
        let h = diff(b"", b"x\ny\n");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].counts(), (2, 0));
        assert_eq!(counts(b"x\n", b""), (0, 1));
        assert_eq!(
            counts(b"x\n", b"x"),
            (1, 1),
            "git counts the newline change as 1/1"
        );
        assert_eq!(apply_hunks(b"", &h, &[0]), b"x\ny\n");
        let m = Hunk::mode_change("100644", "100755");
        assert_eq!(m.counts(), (1, 1));
    }

    // -------- property tests --------

    fn unique_lines(n: usize, terminated: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..n {
            out.extend_from_slice(format!("line-{i}\n").as_bytes());
        }
        if !terminated && !out.is_empty() {
            out.pop();
        }
        out
    }

    #[derive(Debug, Clone)]
    enum Edit {
        Insert(usize, usize),
        Delete(usize, usize),
        Replace(usize, usize, usize),
    }

    /// Apply an edit script to a line vector, drawing new lines from a disjoint pool.
    fn apply_edits(base: &[Vec<u8>], edits: &[Edit], fresh: &mut usize) -> Vec<Vec<u8>> {
        let mut cur: Vec<Vec<u8>> = base.to_vec();
        let new_line = |fresh: &mut usize| {
            *fresh += 1;
            format!("new-{}\n", *fresh).into_bytes()
        };
        for e in edits {
            match *e {
                Edit::Insert(at, n) => {
                    let at = at.min(cur.len());
                    for _ in 0..n {
                        cur.insert(at, new_line(fresh));
                    }
                }
                Edit::Delete(at, n) => {
                    if cur.is_empty() {
                        continue;
                    }
                    let at = at.min(cur.len() - 1);
                    let end = (at + n).min(cur.len());
                    cur.drain(at..end);
                }
                Edit::Replace(at, n, m) => {
                    if cur.is_empty() {
                        continue;
                    }
                    let at = at.min(cur.len() - 1);
                    let end = (at + n).min(cur.len());
                    cur.drain(at..end);
                    for _ in 0..m {
                        cur.insert(at, new_line(fresh));
                    }
                }
            }
        }
        cur
    }

    fn join(lines: &[Vec<u8>], terminated: bool) -> Vec<u8> {
        let mut out: Vec<u8> = lines.concat();
        if !terminated && out.last() == Some(&b'\n') {
            out.pop();
        }
        out
    }

    fn edit_strategy() -> impl Strategy<Value = Edit> {
        prop_oneof![
            (0..40usize, 1..4usize).prop_map(|(a, n)| Edit::Insert(a, n)),
            (0..40usize, 1..4usize).prop_map(|(a, n)| Edit::Delete(a, n)),
            (0..40usize, 1..4usize, 1..4usize).prop_map(|(a, n, m)| Edit::Replace(a, n, m)),
        ]
    }

    fn case_strategy() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
        (
            0..40usize,
            any::<bool>(),
            any::<bool>(),
            prop::collection::vec(edit_strategy(), 0..6),
            any::<bool>(),
        )
            .prop_map(|(n, base_term, cur_term, edits, empty_cur)| {
                let baseline = unique_lines(n, base_term);
                let base_lines: Vec<Vec<u8>> = split_lines(&unique_lines(n, true))
                    .iter()
                    .map(|l| l.to_vec())
                    .collect();
                let mut fresh = 0;
                let cur_lines = if empty_cur {
                    Vec::new()
                } else {
                    apply_edits(&base_lines, &edits, &mut fresh)
                };
                (baseline, join(&cur_lines, cur_term))
            })
    }

    fn sorted_changes(hunks: &[Hunk]) -> Vec<(Tag, Vec<u8>)> {
        let mut v: Vec<(Tag, Vec<u8>)> = hunks.iter().flat_map(Hunk::change_lines).collect();
        v.sort();
        v
    }

    /// The independent oracle: apply one hunk at a time from the **last** hunk backwards,
    /// so earlier offsets are never invalidated.
    fn apply_one_at_a_time(baseline: &[u8], hunks: &[Hunk], selected: &[usize]) -> Vec<u8> {
        let mut lines: Vec<Vec<u8>> = split_lines(baseline).iter().map(|l| l.to_vec()).collect();
        let mut chosen: Vec<&Hunk> = hunks
            .iter()
            .filter(|h| selected.contains(&h.index))
            .collect();
        chosen.sort_by_key(|h| std::cmp::Reverse(h.old_range.start));
        for h in chosen {
            let replacement: Vec<Vec<u8>> = h
                .lines
                .iter()
                .filter(|(t, _)| *t != Tag::Delete)
                .map(|(_, b)| b.clone())
                .collect();
            lines.splice(h.old_range.clone(), replacement);
        }
        lines.concat()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 1000, .. ProptestConfig::default() })]

        #[test]
        fn hunks_accept_then_recompute_never_resurrects_or_drops((baseline, current) in case_strategy()) {
            let hunks = diff(&baseline, &current);
            let all = sorted_changes(&hunks);
            for h in &hunks {
                let accepted = apply_hunks(&baseline, &hunks, &[h.index]);
                let remaining = sorted_changes(&diff(&accepted, &current));
                let mut expected = all.clone();
                for line in h.change_lines() {
                    let pos = expected.iter().position(|x| *x == line).expect("present");
                    expected.remove(pos);
                }
                prop_assert_eq!(remaining, expected, "hunk {} of {}", h.index, hunks.len());
            }
        }

        #[test]
        fn hunks_apply_all_in_any_order_yields_current(
            (baseline, current) in case_strategy(),
            mask in prop::collection::vec(any::<bool>(), 0..12),
        ) {
            let hunks = diff(&baseline, &current);
            prop_assert_eq!(counts(&baseline, &current), hunks.iter().fold((0, 0), |(a, d), h| {
                let (ha, hd) = h.counts();
                (a + ha, d + hd)
            }));
            let all: Vec<usize> = (0..hunks.len()).collect();
            prop_assert_eq!(apply_hunks(&baseline, &hunks, &all), current.clone());
            let mut rev = all.clone();
            rev.reverse();
            prop_assert_eq!(apply_hunks(&baseline, &hunks, &rev), current.clone());
            let subset: Vec<usize> = all.iter().copied().filter(|i| mask.get(*i).copied().unwrap_or(false)).collect();
            prop_assert_eq!(
                apply_hunks(&baseline, &hunks, &subset),
                apply_one_at_a_time(&baseline, &hunks, &subset)
            );
            prop_assert_eq!(apply_hunks(&baseline, &hunks, &[]), baseline.clone());
        }
    }
}

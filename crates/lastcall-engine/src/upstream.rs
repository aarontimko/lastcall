//! The upstream annotation classifier (docs/spec/00-spec.md §6.4 "Upstream annotation";
//! kickoff deliverable 9). A port of the harness's `lc_upstream_paths` and the per-path
//! rule in `lc_pile`, with the cost bounded per head instead of per commit.
//!
//! Heads = `HEAD` (+ `MERGE_HEAD`). Per head the range is `seen_head..head` when
//! `seen_head` is an ancestor, else `merge-base(seen_head, head)..head`; no merge-base →
//! no annotation. A commit is **upstream** iff it is reachable from a remote-tracking ref
//! and neither its author nor its committer email is the user's; a local merge's `--cc`
//! paths go to **M**, other local commits' paths to **L**, upstream paths to **U**. A
//! pending path in U∖(L∪M) whose content equals the blob at some head is `upstream`; in U
//! otherwise it is `mixed`; else it is plain. Detached with no upstream, shallow, or no
//! remotes → nothing is annotated.

use std::collections::{BTreeSet, HashSet};

use crate::git::{GitError, Oid, RepoGit};
use crate::headstate::HeadState;
use crate::scan::{Annotation, Pile};

/// What the classification was computed for; recomputed only when it changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassifyKey {
    pub seen_head: Option<Oid>,
    pub head: Option<Oid>,
    pub merge_head: Option<Oid>,
}

impl ClassifyKey {
    pub fn of(seen_head: Option<&Oid>, state: &HeadState) -> Self {
        Self {
            seen_head: seen_head.cloned(),
            head: state.head.clone(),
            merge_head: state.merge_head.clone(),
        }
    }
}

/// Path sets for one `(seen_head, heads)` combination.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Classification {
    pub key: ClassifyKey,
    /// Heads whose blobs a candidate row is compared against.
    pub heads: Vec<Oid>,
    /// Touched only by upstream commits (U ∖ (L ∪ M)).
    pub upstream_only: BTreeSet<Vec<u8>>,
    /// Touched by upstream **and** local commits (U ∩ (L ∪ M)).
    pub both: BTreeSet<Vec<u8>>,
}

impl Classification {
    pub fn is_empty(&self) -> bool {
        self.upstream_only.is_empty() && self.both.is_empty()
    }
}

/// Memoizes [`classify`] on its key.
#[derive(Debug, Default)]
pub struct Classifier {
    cached: Option<Classification>,
}

impl Classifier {
    pub fn get(
        &mut self,
        rg: &RepoGit,
        seen_head: Option<&Oid>,
        state: &HeadState,
        user_email: Option<&str>,
    ) -> Result<&Classification, GitError> {
        let key = ClassifyKey::of(seen_head, state);
        if self.cached.as_ref().is_none_or(|c| c.key != key) {
            self.cached = Some(classify(rg, seen_head, state, user_email)?);
        }
        Ok(self.cached.as_ref().expect("just filled"))
    }

    pub fn invalidate(&mut self) {
        self.cached = None;
    }
}

/// One `git log` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub commit: Oid,
    pub parents: Vec<Oid>,
    pub author_email: String,
    pub committer_email: String,
    pub paths: Vec<Vec<u8>>,
}

const LOG_FORMAT: &str = "--format=%x01%H%x02%P%x02%ae%x02%ce";

/// Parse `log --cc --name-only -z --format=<LOG_FORMAT>`: records start with `\x01`, the
/// header's fields are `\x02`-separated and end at the first NUL/newline, and the paths
/// follow NUL-separated.
pub fn parse_log(bytes: &[u8]) -> Vec<LogEntry> {
    let mut out = Vec::new();
    for chunk in bytes.split(|b| *b == 0x01).filter(|c| !c.is_empty()) {
        let header_end = chunk
            .iter()
            .position(|b| *b == 0 || *b == b'\n')
            .unwrap_or(chunk.len());
        let header = String::from_utf8_lossy(&chunk[..header_end]);
        let fields: Vec<&str> = header.split('\x02').collect();
        if fields.len() < 4 {
            continue;
        }
        let Some(commit) = Oid::parse(fields[0].trim()) else {
            continue;
        };
        let parents = fields[1]
            .split_whitespace()
            .filter_map(Oid::parse)
            .collect();
        let paths = chunk[header_end..]
            .split(|b| *b == 0 || *b == b'\n')
            .filter(|p| !p.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        out.push(LogEntry {
            commit,
            parents,
            author_email: fields[2].trim().to_owned(),
            committer_email: fields[3].trim().to_owned(),
            paths,
        });
    }
    out
}

/// Compute the classification now (see the module docs).
pub fn classify(
    rg: &RepoGit,
    seen_head: Option<&Oid>,
    state: &HeadState,
    user_email: Option<&str>,
) -> Result<Classification, GitError> {
    let key = ClassifyKey::of(seen_head, state);
    let mut heads: Vec<Oid> = Vec::new();
    heads.extend(state.head.clone());
    heads.extend(state.merge_head.clone());
    let mut class = Classification {
        key,
        heads: heads.clone(),
        ..Default::default()
    };
    let Some(seen_head) = seen_head else {
        return Ok(class);
    };
    if state.shallow || heads.is_empty() {
        return Ok(class);
    }
    let mut u: HashSet<Vec<u8>> = HashSet::new();
    let mut lm: HashSet<Vec<u8>> = HashSet::new();
    for head in &heads {
        let is_ancestor = rg
            .run_raw(
                &[
                    "merge-base",
                    "--is-ancestor",
                    seen_head.as_str(),
                    head.as_str(),
                ],
                None,
            )?
            .success();
        let base = if is_ancestor {
            seen_head.clone()
        } else {
            let out = rg.run_raw(&["merge-base", seen_head.as_str(), head.as_str()], None)?;
            match Oid::parse(&out.stdout_trimmed()) {
                Some(b) if out.success() => b,
                _ => continue, // no merge-base: no annotation for this head (C7)
            }
        };
        let range = format!("{}..{}", base.as_str(), head.as_str());
        let not_remote: HashSet<String> =
            String::from_utf8_lossy(&rg.run(&["rev-list", &range, "--not", "--remotes"])?)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
        let log = rg.run(&["log", "--cc", "--name-only", "-z", LOG_FORMAT, &range])?;
        for entry in parse_log(&log) {
            let remote_reachable = !not_remote.contains(entry.commit.as_str());
            let mine = user_email.is_some_and(|e| {
                !e.is_empty() && (entry.author_email == e || entry.committer_email == e)
            });
            if remote_reachable && !mine {
                u.extend(entry.paths);
            } else {
                // A local merge's `--cc` paths (M) and other local commits' paths (L)
                // both demote a path to "mixed".
                lm.extend(entry.paths);
            }
        }
    }
    for p in u {
        if lm.contains(&p) {
            class.both.insert(p);
        } else {
            class.upstream_only.insert(p);
        }
    }
    Ok(class)
}

/// Apply the classification to a pile's rows: `upstream` when the row is in
/// U∖(L∪M) and its content equals the path's blob at some head, `mixed` when it is in
/// U otherwise, else untouched.
pub fn annotate(pile: &mut Pile, class: &Classification, rg: &RepoGit) -> Result<(), GitError> {
    if class.is_empty() {
        return Ok(());
    }
    // One cat-file --batch-check for every (head, path) pair we must compare.
    let mut specs: Vec<(usize, Vec<u8>)> = Vec::new();
    for (i, row) in pile.rows.iter().enumerate() {
        if class.upstream_only.contains(&row.path) && row.current.is_some() {
            for head in &class.heads {
                let mut spec = head.as_str().as_bytes().to_vec();
                spec.push(b':');
                spec.extend_from_slice(&row.path);
                specs.push((i, spec));
            }
        }
    }
    let mut stdin: Vec<u8> = Vec::new();
    for (_, spec) in &specs {
        stdin.extend_from_slice(spec);
        stdin.push(b'\n');
    }
    let blobs: Vec<Option<Oid>> = if specs.is_empty() {
        Vec::new()
    } else {
        let out = rg.run_stdin(&["cat-file", "--batch-check"], &stdin)?;
        parse_batch_oids(&out)
    };
    let mut matches: HashSet<usize> = HashSet::new();
    for (k, (i, _)) in specs.iter().enumerate() {
        if let Some(Some(oid)) = blobs.get(k)
            && pile.rows[*i]
                .current
                .as_ref()
                .is_some_and(|c| &c.oid == oid)
        {
            matches.insert(*i);
        }
    }
    for (i, row) in pile.rows.iter_mut().enumerate() {
        if class.upstream_only.contains(&row.path) {
            row.annotation = Some(if matches.contains(&i) {
                Annotation::Upstream
            } else {
                Annotation::Mixed
            });
        } else if class.both.contains(&row.path) {
            row.annotation = Some(Annotation::Mixed);
        }
    }
    Ok(())
}

/// `--batch-check` output, one line per input: `<oid> <type> <size>` → `Some(oid)`,
/// anything else (`<spec> missing`, a tree) → `None`. Paths may contain spaces, so parse
/// from the line's shape rather than its token count.
pub fn parse_batch_oids(bytes: &[u8]) -> Vec<Option<Oid>> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|line| {
            let s = String::from_utf8_lossy(line);
            let mut it = s.split(' ');
            match (it.next(), it.next(), it.next()) {
                (Some(oid), Some("blob"), Some(_)) => Oid::parse(oid),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_parse_log_records() {
        let h1 = "1".repeat(40);
        let h2 = "2".repeat(40);
        let p = "3".repeat(40);
        let bytes = format!(
            "\x01{h1}\x02{p} {h2}\x02a@x\x02c@x\0\nsrc/a.rs\0b c.txt\0\x01{h2}\x02\x02me@x\x02me@x\0\nonly.rs\0"
        );
        let log = parse_log(bytes.as_bytes());
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].parents.len(), 2, "merge commit");
        assert_eq!(
            log[0].paths,
            vec![b"src/a.rs".to_vec(), b"b c.txt".to_vec()]
        );
        assert_eq!(log[0].author_email, "a@x");
        assert!(log[1].parents.is_empty(), "root commit");
        assert_eq!(log[1].paths, vec![b"only.rs".to_vec()]);
        assert_eq!(log[1].committer_email, "me@x");
    }

    #[test]
    fn upstream_parse_batch_oids_handles_missing_and_spaces() {
        let oid = "a".repeat(40);
        let bytes = format!("{oid} blob 12\nabc:some path.txt missing\n{oid} tree 40\n");
        let got = parse_batch_oids(bytes.as_bytes());
        assert_eq!(got, vec![Oid::parse(&oid), None, None]);
    }
}

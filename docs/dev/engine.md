# The review engine (Phase 2)

`crates/lastcall-engine` is the headless half of lastcall: it discovers roots, keeps one
ledger per root, computes the pending pile, annotates it against upstream, and applies
compare-and-swap accepts. No terminal code, no process environment — everything arrives
through the injected `Env` and the loaded config. The binary's `status` and `watch`
subcommands are thin wrappers (`crates/lastcall/src/commands/{status,watch}.rs`).

The contract it implements is `docs/spec/00-spec.md` §6 (frozen); the scenarios it must
satisfy are `docs/spec/01-scenarios.md`, one integration test per ID
(`crates/lastcall-engine/tests/test_integration_scenarios_{a..f}.rs`, `just test-scenarios`).

## The model in one paragraph

The user has *seen* content, not commits. A root's **seen tree** is a git tree object in a
private store; per-path **overrides** in `ledger.json` sit on top of it (a blob oid the user
accepted, `null` for "seen as absent", or a flag). The **baseline** of a path is:
override blob → override `null` (absent) → seen-tree entry → empty. **Pending** is
`diff(baseline, worktree)`: a row exists iff the baseline and the live file differ by oid or
mode. HEAD is never a baseline input; it is consulted only for first sight, the transition
notices (`headstate.rs`) and the upstream annotation (`upstream.rs`). Every accept is a CAS
against the oid/mode the user was shown; on any doubt the engine shows *more*, never less.

## Storage walkthrough

State lives under `LASTCALL_STATE_DIR` → `$XDG_STATE_HOME/lastcall` → `~/.local/state/lastcall`.
Ids are the first 16 hex chars of SHA-256 over the canonicalized path.

```text
<state>/roots/<parent-id>/meta.json                  # the parent dir this group was discovered under
<state>/roots/<parent-id>/repos/<root-id>/
    ledger.json      # schema 1.0 (§6.2): seen_tree, seen_at, overrides
    store/           # bare git repo; objects/info/alternates → the user's objects dir (git roots)
    index            # private index seeded from the seen tree (a cache, never truth)
    index.tree       # the tree `index` was seeded from; mismatch with the ledger → reseed
    index.<pid>.tmp  # scratch index for accept-all / compaction folds, one per process;
                     # unlinked after each fold, on engine drop, and (an hour old, another
                     # pid, never on Windows) by the sweep at the next store open
    lock             # flock for ledger writes (40 × 50 ms = 2 s, then the op errors); every
                     # write reloads the on-disk ledger under it and replays the in-memory change,
                     # so two engines over one root keep each other's accepts
```

Look at a root's state with plain tools (replace the ids with `ls` output):

```sh
ls ~/.local/state/lastcall/roots/*/repos/*
jq . ~/.local/state/lastcall/roots/<parent>/repos/<root>/ledger.json
cd ~/.local/state/lastcall/roots/<parent>/repos/<root>
GIT_DIR=store git ls-tree -r "$(jq -r .seen_tree ledger.json)"          # the seen tree
GIT_DIR=store git cat-file -p "$(jq -r '.overrides["path/to/file"].blob' ledger.json)"  # an override blob
GIT_DIR=store git ls-tree -r "$(cat index.tree)" | head                # what the index was seeded from
```

`ledger.json` is written as `ledger.json.tmp` + `fsync` + `rename` under `lock`. A stale
`ledger.json.tmp` found at open (a crash between the two steps, E1) is removed with a notice
— judged under the lock, so a live writer's tmp is never mistaken for a stale one.
An unreadable ledger is moved aside to `ledger.json.unreadable-<secs>-<n>` and the root
opens with `seen_tree = null` (every path pending); that ledger is written at once so the next
open finds it — it is *not* first sight, because first sight at the current HEAD would hide
everything committed since. Open reads a present ledger without the lock; when there is none
it takes the lock and looks again before writing anything, so two processes opening the same
never-seen root cannot clobber each other, and a moved-aside sibling with no ledger beside it
(the other process has not written its replacement yet) opens as unreadable, never as first
sight.

The store is never garbage-collected. Nothing in it is anchored by a ref, so `gc`, `prune`,
`repack -d` or `fsck --lost-found` would delete the seen tree and every override blob. It
grows by one loose object per scanned version of each changed file; this is accepted for v1
(the hardening shape is `refs/lastcall/seen` plus an overrides tree ref, after which gc is
safe).

## The two git runners

`crates/lastcall-engine/src/git.rs` is the only file that spawns `git` (gate grep
`rg -n 'Command::new\("git"\)' crates/lastcall-engine/src`). Both runners build on the
injected `Env`, set `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`, run every
path-producing command with `-z`, and scrub `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`,
`GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_COMMON_DIR`,
`GIT_CEILING_DIRECTORIES`, `GIT_NAMESPACE`, `GIT_PREFIX`, `GIT_INDEX_VERSION`,
`GIT_CONFIG_PARAMETERS` and `GIT_CONFIG_COUNT` (which disarms every `GIT_CONFIG_KEY_n`)
from the child after the overlay (`git::SCRUBBED_VARS`). Every child also gets
`-c core.fsmonitor=false -c core.untrackedCache=false -c core.splitIndex=false`
(`git::NEUTRALIZED_CONFIG`), and the store's own config carries the same three keys, written
at every open beside the copied `core.autocrlf`/`eol`/`filemode`/`ignorecase`: a user's global
`core.fsmonitor = true` otherwise makes `update-index --refresh` under our `GIT_DIR` wait on a
daemon that never answers (and starts `fsmonitor--daemon`s as a side effect), and the other
two would put index extensions into the private index that the seed/refresh path does not
manage. A child's stdin (`hash-object --stdin-paths`, `cat-file --batch`,
`update-index --index-info`) is written from its own scoped thread while the runner drains
stdout and stderr: a batch command answers each line as it reads it, so feeding the whole
input before reading any output deadlocks once both 64 KiB pipes are full — a few thousand
paths, which is how the Phase 4 bench's 50,000-file drop first hung the scan
(`git_run_command_feeds_stdin_while_draining_stdout` round-trips 300 KiB through `cat`).

| runner | cwd | env | may write? | used for |
|---|---|---|---|---|
| `StoreGit` | the root | `GIT_DIR=<store>`, `GIT_WORK_TREE=<root>`, `GIT_INDEX_FILE=<index>` explicit on every call | yes, to **our** store and index only | `hash-object -w --stdin-paths`, `read-tree`, `write-tree`, `ls-tree`, `cat-file --batch-check`, `cat-file --batch` (one call per scan for every rendered blob), `diff-files`, `ls-files --others`, `update-index --refresh`, `config` (the neutralized keys) |
| `RepoGit` | the root | the three variables removed | **never** | read-only inspection of the user's repository |

`RepoGit::allowed` is a closed allowlist; `git.rs` unit tests are the guard:

| verb | allowed form |
|---|---|
| `--version` | alone |
| `rev-parse`, `rev-list`, `merge-base`, `diff-tree`, `cat-file`, `for-each-ref` | any arguments |
| `symbolic-ref` | exactly one ref (the two-ref form writes) |
| `config` | `--get` only |
| `ls-files` | only with `-v`, `-u`, `--stage`/`-s` (skip-worktree bits and conflict stages are *read* from the user's index, D6/C4) |
| `log` | only with a `--format` |
| `worktree` | `list --porcelain` only |

Refused by construction: `status`, `diff`, `add`, `update-index`, `checkout`, `stash`,
`commit`, `reset`, `gc` — the user's index and worktree are theirs.

## Scan pipeline in ten lines

0. Re-read `ledger.json` if its (mtime, length, inode) changed since it was loaded — another process accepted or folded — so a long-running engine never scans against a stale baseline (the read is unlocked; writes are rename-atomic). A file that no longer parses leaves the loaded ledger in place.
1. Refresh the private index (`update-index --refresh`); a held `index.lock` is retried 3 × 50 ms, then the scan proceeds unrefreshed (over-report at worst; also when git cannot be spawned).
2. Candidates = `diff-files` paths ∪ `ls-files --others` files ∪ every override path ∪ (case-insensitive roots) seen-tree names absent byte-exactly from their directory.
3. Minus paths tagged skip-worktree in the **user's** index *that are absent from the worktree* (D6: a sparse cone; a present one is a real edit and stays); minus `others` entries under a nested repo (D9) or a sibling draft root. The skip-worktree filter applies to every candidate whichever list it came from: an absent cone path is a cone even when an override or `diff-files` names it.
4. `current` per path = `lstat` → `Absent` | `Unhashable(reason)` | `{oid, mode}`; hashed in one `hash-object -w --stdin-paths` call from the root (so `text=auto` and clean filters apply). `Unhashable` is always a row. **Row cap** (Phase 4): the candidates are partitioned *before* hashing — every override path first (they are always shown: a dropped override would silently un-pend a file), then the rest in path order in batches until `row_cap` non-override rows have materialised (`EngineOptions::row_cap`, default `DEFAULT_ROW_CAP = 10_000`; no config key). The remainder is never hashed: its count is `pile.omitted` and the notice `<shown> files shown · <omitted> more changed paths not scanned (first <cap> by path)` rides along (numbers with thousands separators: `first 10,000 by path`); an accept-all over such a pile folds the shown rows only, and the next scan shows the next `row_cap` paths. With overrides present a scan issues two `hash-object` batches instead of one.
5. `baseline` per path from the ledger (override blob → override null → seen tree → empty); a missing override blob or an unparsable override falls to the tree with a notice (E2).
6. Row iff baseline ≠ current by oid or mode (D1); on `core.filemode=false` roots the executable bit is normalized away on both sides, so a mode-only row cannot appear there.
7. Every baseline and current blob of every row fetched in **one** `cat-file --batch` (2,000 unseen files cost 16 git processes per scan, not 2,000); hunks from a byte diff of the two blobs (`hunks.rs`); binary (NUL in the first 8000 bytes) or ≥ `collapse_size_bytes` or matching `collapsed_globs` → collapsed (D7/D8). A blob the batch cannot produce renders that row without content and a notice.
8. Conflict state from the user's `ls-files -u` (C4); the override's flag; rename pairing (D5) — presentation only, the ledger stores delete + add. The pairing reads the **pile's rows**: a temp index (`index.tmp`) holds exactly the `deleted` rows at their baselines (`read-tree --empty` + `update-index --index-info`), the `added` rows are `add -N`ed, then `diff -M -z --name-status`. It is not a copy of the private index — that is the seen tree, and a path the user accepted as deleted (override `null`, no row) or a deleted row whose baseline is an override blob would otherwise pair or score differently before and after a compaction that changes no baseline.
9. Annotation (`upstream.rs`): heads = HEAD (+ `MERGE_HEAD`); range `seen_head..head` or from the merge-base; commits reachable from a remote ref by someone else are upstream; a pending path whose content equals a head's blob is `upstream`, touched upstream but different is `mixed`, otherwise plain. No merge-base, detached with no upstream, shallow, or no remotes → nothing annotated (C7).
10. Notices from every step ride along in `pile.notices`; nothing after step 6 removes a row.

## The fail-open ladder

Every rung shows *more* than the truth, never less, and says why in a notice:

| condition | behaviour |
|---|---|
| no ledger | first sight: seen tree = `HEAD^{tree}` (`null` before the first commit); drafts follow `draft_initial` (`seen` snapshots the dir, `pending` anchors to no tree) |
| a sibling ledger whose recorded root is gone (the repo was moved, E4) | first sight for the new path, a notice naming the old root; old state is kept |
| unreadable ledger / unknown schema major | moved aside, `seen_tree = null`, every path pending |
| a ledger whose recorded `root` is another path (copied state, a moved `LASTCALL_STATE_DIR` entry) | the same: another root's state is never adopted; moved aside, `seen_tree = null`, notice `recorded root <old> is not <root>` |
| `ledger.json` rewritten by another process while this engine runs | adopted at the next scan (stamp compare + re-read); a seen tree the store no longer has is treated as `null` |
| stale `ledger.json.tmp` | removed at open, notice (E1) |
| seen tree not resolvable in the store (user ran `git gc`) | treated as `null`, notice; the next accept persists `null` and accept-all seeds from the empty tree |
| HEAD cannot be inspected (corrupt `.git/HEAD`, git-dir unreadable) | rows unchanged, no annotation, notice `head inspection skipped` |
| override blob missing (E3) or override unparsable (E2) | that path resolves to the seen-tree entry |
| `index` / `index.tree` missing, mismatched, or unreadable (empty, garbage, truncated) | reseeded from the seen tree; the pile is identical |
| `index.lock` held by another process | scan runs unrefreshed |
| a path that cannot be hashed (unreadable, a socket, `git-lfs` missing) | an `Unhashable` row |
| accept whose rendered oid/mode/baseline (oid **and** mode for hunks, so a stale mode hunk cannot apply twice) no longer matches the live file | refused, nothing written; the next scan shows the new state (A5/A6) |
| ledger lock busy after 40 × 50 ms (2 s) | the op errors; nothing is written unlocked, and the TUI says `ledger busy in <root> — try again` with the row still pending |

## Accepting through the engine (Phase 4)

The binary's TUI (and any other client) accepts through one method, never through `Ops`
directly:

```rust
pub enum AcceptRequest {
    Hunk { rendered: Rendered, hunks: Vec<Hunk>, index: usize },
    File(Rendered),        // `rendered.oid == None` is the deletion form
    Group(Vec<Rendered>),  // one ledger write for the whole group
    All(Pile),             // the snapshot the user confirmed
}
pub struct Accepted { pub outcome: Outcome, pub seq: u64, pub pile: Pile }
impl Engine {
    pub fn accept(&mut self, root: &Path, req: AcceptRequest) -> Result<Accepted, EngineError>;
}
```

`accept` is **one critical section**: the op (`Ops::accept_hunk` / `accept_file` /
`accept_deletion` / `accept_group` / `accept_all`, exactly as before) and then a rescan of
that root run without releasing the engine's mutex (the watcher's `blocking`), so the
`Accepted.pile` is the post-accept pile and no scan scheduled in between can publish a
stale one first. A CAS refusal is data (`Outcome::Refused`, `docs/spec/00-spec.md` §6.3)
and still `Ok`: the rescan runs and shows the live state. `Err` is storage failure only
(`EngineError::Ops`); on `Err` the root's in-memory ledger is discarded and re-read from
disk, so a write that died after staging (E1's `AfterLedgerTmpWrite`) leaves the engine on
the committed ledger. `accept_with(root, req, &dyn FaultInjector)` is the same method with
the E1 fault seam; production passes `NoFault`.

**`Err` after a committed op.** The two halves of the critical section fail differently.
An `Err` from the op itself (`EngineError::Ops`) means the ledger's rename did not land:
nothing was accepted. But the rescan that follows can fail too (`self.scan(root)?` — a git
or io error, or the root gone from the engine), and *that* `Err` arrives after the op
committed: the ledger on disk already holds the accept, and the engine's in-memory ledger
is the committed one (`Ops::commit` merges it with disk and tmp-writes it under the lock,
then renames; the next `scan` reloads from disk anyway). A client that reports the
`Err` as "the accept failed" is therefore wrong about the op and right only about the
pile: the op is durable; the next successful scan of that root — the watcher's, a
refresh, `status` — shows the post-accept pile; and a retry with the same request is
harmless, since it re-folds the same rendered bytes onto the same ledger (a hunk retry is
CAS-refused as `BaselineMoved`, a file/group/all retry re-blesses what is already
blessed). No engine change is planned for this: the `Err` is honest about what the caller
did not get (a pile), and the ledger is the truth either way.

**Scan seq.** `Engine::scan_seq()` is an engine-global counter stepped once per successful
`scan` (a failed scan does not step it) and read under the same lock as the scan it
numbers. Every publisher of a pile carries it: `EngineEvent::Pile { root, seq, pile }`,
`HeadChange.seq` (the scan `inspect_head` ran), `scan_all`'s `(root, seq, result)` tuples (a
failed root reports the engine's current seq), and `Accepted.seq`. It is the seam for a
client that applies piles from several sources — a refresh, the watcher, its own accept —
to drop one that is older than what it already shows (the TUI's reducer, Phase 4
deliverable 5).

## `status --json` schema (`status_version: 1`)

```json
{
  "status_version": 1,
  "notices": ["engine-level notices (config resolution, ad-hoc cwd, moved roots)"],
  "roots": [
    {
      "root": "/abs/path", "kind": "git | draft", "parent": "/abs/parent-dir",
      "badge": null | {"worktree_of": "/abs/main"} | {"nested_in": "/abs/outer"},
      "head": "<oid> | null", "branch": "main | null",
      "remote": "org/repo | null",
      "in_progress": null | "merge" | "rebase" | "cherry-pick" | "revert",
      "seen_tree": "<oid> | null", "seen_head": "<oid> | null",
      "pending": [
        {
          "path": "rel/path", "change": "modified | added | deleted | mode | typechange | unreadable",
          "baseline": {"oid": "…", "mode": "100644"} | null,
          "current":  {"oid": "…", "mode": "100644"} | null,
          "added": 1, "deleted": 1, "hunks": 1,
          "annotation": null | "upstream" | "mixed",
          "conflicted": false,
          "collapsed": null | "glob" | "binary" | "size",
          "flag": null | {"note": "…"},
          "rename": null | {"from": "old", "similarity": 90} | {"to": "new", "similarity": 90}
        }
      ],
      "omitted": 0,
      "groups": [{"kind": "upstream", "paths": ["u1"]}],
      "notices": ["root-level and scan notices"]
    }
  ]
}
```

Roots are sorted by path bytes, rows by path bytes; paths are lossy UTF-8 (a non-UTF-8 path
renders with U+FFFD but is still a row). `remote` (additive, Phase 3 / Amendment v1.3;
`status_version` stays 1) is the `org/repo` slug of `remote.origin.url` — read with the
allowlisted `git config --get`, not `git remote get-url origin`, which differs only under
`url.<base>.insteadOf` — normalized by `lastcall_engine::engine::remote_slug`: `git@host:org/repo(.git)`,
`ssh://…/org/repo`, `https://host/org/repo` and scp-like `host:org/repo` give `org/repo`;
no remote, a local-path or `file://` origin (the fixtures' origins), and anything unparsable
give `null`. The committed example is
`crates/lastcall/tests/golden/status_multi_repo.json` (two repos and a draft dir, produced by
`lastcall_testkit::fixture_parent`; `just golden-update` rewrites it).

`omitted` (additive, Phase 4; `status_version` stays 1 — an Amendment v1.4 candidate) is
the number of changed paths beyond the row cap that this scan did not hash (see step 4 of
the pipeline); `0` whenever everything changed is in `pending`, and the root's `notices`
name the cap when it is not. `pending` never holds more than `row_cap` non-override rows.
Readers that predate the field ignore it; `Pile::omitted` deserializes as `0` when absent.

`lastcall status [--root <path>]...` scans only the roots the given paths resolve to (a path
inside a root selects that root) and nothing else; a path that is not inside any watched root
is `lastcall: --root <path>: not a watched root` on stderr and exit 1, never an empty report.
Without `--root`, exit 1 means only that the engine could not open (a per-root scan failure
is a notice).

## Watching

`Engine::run` (`watcher.rs`) puts one recursive `notify` watch on each **parent dir** (plus
any root not under one, and any git dir outside them — a linked worktree's common dir). The
watches are installed on a blocking task while the initial scans run: on macOS every `watch`
call re-registers the FSEvents stream, which took ~1.8 s per call on the development machine,
and the first pile must not wait for that. When the watch goes live every root is scanned
and inspected once more (nothing from the gap is missed) and a `watching <dirs> (N roots)`
notice is emitted; the rescan backstop counts from there. Git-dir events are routed to the root whose
git dir (or common dir) is the *longest* prefix of the path — a linked worktree's git dir is a
subdirectory of the main worktree's (`<main>/.git/worktrees/<name>`), and at equal length a
root's own git dir beats another root's common dir — then filtered to an allowlist (`HEAD`,
`index`, `ORIG_HEAD`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `refs/`,
`rebase-merge/`, `rebase-apply/`, `logs/`), worktree events are debounced on the trailing
edge (750 ms), and two polling backstops remain (HEAD every 10 s, root discovery every 30 s).
`scan_all` re-runs discovery only when a scan saw a root's set of nested repositories change,
not on every tick while one exists. Events only *schedule* work: every scan, head inspection and rescan runs on
`spawn_blocking` under the engine's mutex, and the result is published as an `EngineEvent`
(`Pile { root, seq, pile }`, `Head` with its transition notice and the seq of the scan it
ran, `RootsChanged`, `Notice`). `ignore_globs` scope
the watcher only — an ignored path never wakes a scan, but the next scan still shows the
tracked edit. `lastcall watch [--json] [--exit-after N] [--poll N]` prints one line per
event (a pile line is `<root> #<seq>  <n> pending`; the JSON form carries `seq` and
`omitted`); `--poll N` shortens both backstops to `N` s for hosts whose filesystem events are late
or missing (the development machine's fseventsd delivered nothing during Phase 2; the
`watcher_worktree_edit_schedules_a_scan_without_polling` test in
`crates/lastcall-engine/tests/test_integration_watcher.rs` proves delivery where it works and
skips with a reason where it does not). `just probe-watch` demonstrates the B1
notice (`committed on main (1 commit)`) arriving while the pending row stays.

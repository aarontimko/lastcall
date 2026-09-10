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

Phase 5 cut the per-root spawn count on the read side. The store's config is read with one
`config --list -z` (`ConfigList::parse`: NUL-separated records, the key ends at the first
newline, so `=` and newlines inside a value survive, and a repeated key means last wins)
instead of seven `--get`s. Head inspection batches what it can into one
`rev-parse --path-format=absolute` (`rev_parse_batch(flags, expected_lines)` checks the line
count it got back): `--git-dir --git-common-dir --is-shallow-repository` plus a `--git-path
<name>` per interesting file, six spawns down to four. `symbolic-ref -q --short HEAD` and the
two `-q --verify` calls stay separate because a non-zero exit *is* their answer, and a batch
would lose which line failed. S1's `open_spawns` fell 2,902 → 1,102 (`docs/dev/bench.md`,
run C).

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
| `config` | `--get <key>`, or exactly `--list -z` (Phase 5: one spawn per root instead of seven `--get`s) |
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
8. Conflict state from the user's `ls-files -u` (C4); the override's flag; rename pairing (D5) — presentation only, the ledger stores delete + add. The pairing reads the **pile's rows**: a temp index (`index.<pid>.tmp`, this process's own) holds exactly the `deleted` rows at their baselines (`read-tree --empty` + `update-index --index-info`), the `added` rows are `add -N`ed, then `diff -M -z --name-status`. It is not a copy of the private index — that is the seen tree, and a path the user accepted as deleted (override `null`, no row) or a deleted row whose baseline is an override blob would otherwise pair or score differently before and after a compaction that changes no baseline.
9. Annotation (`upstream.rs`): heads = HEAD (+ `MERGE_HEAD`); range `seen_head..head` or from the merge-base; commits reachable from a remote ref by someone else are upstream; a pending path whose content equals a head's blob is `upstream`, touched upstream but different is `mixed`, otherwise plain. No merge-base, detached with no upstream, shallow, or no remotes → nothing annotated (C7).
10. Notices from every step ride along in `pile.notices`; nothing after step 6 removes a row.

## Many roots at once, many lastcalls at once (Phase 5)

**One pool, two uses.** `rescan` opens newly discovered roots, and `scan_all` scans them, on
a scoped thread pool of `EngineOptions.parallelism` threads (`engine::parallel_map`, built on
`std::thread::scope`). The default is `default_parallelism()` =
`min(available_parallelism(), MAX_PARALLELISM)`, floor 1, and `MAX_PARALLELISM` is **8** —
past that the disk is the limit, not the CPU. There is deliberately no config key for the
width; `LASTCALL_PARALLELISM` is read in the **binary only**
(`crates/lastcall/src/commands/mod.rs`, `parallelism_override`), and it exists so the
`status --json` golden can be produced at width 1 and width 8 and compared byte for byte.
Not a user knob, not read by the engine, not read by the TUI.

What the pool must not change is the answer:

- `parallel_map` returns results **in item order**, never completion order, and `width <= 1`
  (or a single item) runs everything inline with no thread spawned at all — the sequential
  path is then literally the same code, so "identical at 1 and at 8" is a claim about the
  work rather than about two implementations kept in step.
- Everything order-bearing stays serial and outside the pool: the parent's `meta.json` write
  before it; after it, the apply step that inserts roots in path order, assigns `scan_seq`,
  appends notices and ORs `nested_changed`. `EngineEvent::Pile { seq }` ordering
  (§10, Phase 4, "engine-global") is unchanged, because `scan_seq` is still written in the
  apply step and nowhere else. The nested-repo passes stay serial between passes.
- A per-root failure stays per-root: an unopenable root becomes a notice and the rest open.
- No engine lock is held across the pool, and each root's `RootState` is borrowed by exactly
  one thread for the whole call, so the per-root worker needs no lock of its own.

`engine_parallel_open_and_scan_match_the_sequential_run_exactly` and
`engine_parallel_open_reports_an_unopenable_root_as_a_notice` are those claims under test;
`docs/dev/bench.md` run C has what it bought.

**N lastcalls over one state dir.** Amendment v1.2 allows several instances at once — one per
herdr workspace pane is the ordinary case, not the exotic one — so every file a root owns is
either per-process or deliberately shared:

| file | per process or shared | why |
|---|---|---|
| `index.<pid>.tmp` | per process | two concurrent scans would `read-tree` into the same file; a fold owns it for the fold's length |
| `index`, `index.tree` | shared | a cache of the seen tree, and git itself serialises writes to it through `index.lock` |
| `ledger.json` | shared | `lock` serialises writes, and every write reloads the on-disk ledger under it and replays the in-memory change |

The temp index is `paths::temp_index_name(pid)` = `index.<pid>.tmp`, a sibling of `store/` in
the root's state dir. It is unlinked when the fold finishes, again when the `Engine` drops
(the clean-quit path), and — for a file left by *another* process — by `sweep_temp_indexes` at
the next `Store::open`, but only once it has gone an hour untouched (`STALE_TEMP_INDEX`).
There is no liveness probe on purpose: a pid on this machine says nothing (it may have been
reused; the file may have been written inside a container's pid namespace), and a probe would
be a new dependency for a file that costs nothing to leave lying. `is_temp_index_name` matches
`index.<digits>.tmp` and nothing else, so the sweep can never take `index`, `index.tree`, or a
file some later version adds. On Windows only our own is removed. Every failure in the sweep
is silence: a temp index we could not delete is litter, never a reason to fail an open.

The persistent index stays **shared**, which was a decision and not an oversight. It is a
cache; git already serialises it; `PrivateIndex` retries its own `index.lock` 3 × 50 ms
and then scans unrefreshed with the notice `index.lock held by another process; scanned
without refresh` (over-report at worst, which is the fail-open direction).
`engine_two_engines_scan_one_root_concurrently_and_agree` interleaves eight scans across two
engines over one root: all twelve rows identical, and the 3 × 50 ms budget absorbed it with no
exhaustion, so it was left alone.

**The ledger lock budget doubled** in Phase 5: 20 × 50 ms → **40 × 50 ms = 2 s**
(`ledger::LOCK_RETRIES`, `ledger::LOCK_BACKOFF`). With one lastcall per pane, finding another
process mid `read → merge → write tmp → rename` is routine rather than a collision, and one
second was thin on a cold state dir. Above two seconds an accept stops feeling like a
keystroke, so that is where waiting stops and `LedgerError::LockBusy { path, retries }` takes
over. `Ops` carries the budget as `lock: (u32, Duration)` (`ops::DEFAULT_LOCK`) purely so a
test can reach the busy path in 20 ms instead of sleeping the shipping two seconds.

`LockBusy` is an **error, not a refusal**. A refusal means the file moved under us and the
accept is void; this means only that someone else held the lock. Nothing was written, the row
is still pending, and the TUI classifies it as `AcceptFailed::LedgerBusy` — a whole-root
condition rather than a per-row one — and says:

```text
ledger busy in <root> — try again
```

Pressing the same key a moment later is the entire fix.

## Draft roots and collapsed classes (Phase 6)

**A draft root is a directory, not a repo.** `draft_dirs` (globs relative to a parent dir,
or absolute paths) makes a non-git directory a root of its own: discovery lists it beside
the repos, and it gets the same store, ledger and seen tree as a repo does — the store is a
bare object database with **no alternates and no key copy**, since there is no repository
to borrow objects from and no user config to inherit, and `RootKind::Draft` is what tells
the pile apart. First sight follows
`draft_initial`: `seen` (the default) records everything already there as the baseline, so
the root opens at zero and only later edits are pending; `pending` records nothing, so
everything present is a row on the first scan. There is no HEAD, so head inspection,
upstream classification and annotation (pipeline step 9) never run — `scan_root` gates the
whole block on `state.repo`, skipped rather than faked — and neither does the conflict read
of step 8, which needs the user's index. Rename pairing does run: its temp index is our own
store's, not the user's. A
draft root nested inside a repo (a gitignored `_drafts/`) is the interesting case: the repo
scan drops `others` entries under it (pipeline step 3) so one edit is one row, in the draft
root, once.

**A collapsed row is one accept, not a diff.** `render_content` (step 7) runs a ladder and
returns *before* hunks are computed: `collapsed_globs` (the nine common lockfiles by
default) → binary (a NUL byte in the first 8,000 of either side) → size (either side
**strictly larger** than `collapse_size_bytes`, default 512 KiB). The row carries its
`+added −removed` counts and a `Collapsed` tag, no `Hunk`s, and a mode-only change never
synthesises one on a collapsed row (the early return is above that synthesis). Accepting is
whole-row by construction: there is no hunk to point `a` at.

**`Engine::hunks_of(root, row)` is the opt-in escape hatch** — the `e` key's engine half,
never part of a scan, because the point of a collapsed class is that a lockfile rewrite
does not pay a second Myers pass on every rescan. It diffs **the row's own** baseline and
current oids (not a fresh resolution, so the expansion shows exactly the delta the counts
were rendered from even if the file has moved since) and truncates at
`hunks::EXPAND_LINE_CAP` = 2,000 body lines, reporting the remainder as
`Expanded::omitted_lines`. A side the row does not have is legitimately empty; a side it
*does* have whose oid the store cannot produce is `EngineError::MissingBlob`, never an
empty side — reading a missing current blob as empty would draw the live file as one
enormous deletion, hiding exactly what the reviewer pressed `e` to read. The result is a
view: it is never written back onto the `Row`, so `accept_file` and `accept_all` stay
whole-row for a collapsed path.

**One remote-ref listing per scan.** Upstream classification (step 9) is memoized on a
`ClassifyKey` of (seen head, head state, the `for-each-ref refs/remotes` listing). The
listing that *builds* the key is the listing `classify` is given, so a scan runs
`for-each-ref` exactly once and reuses it for every row —
`engine_classification_lists_remote_refs_once_per_scan` counts the argv. The measured
effect is one fewer git process per root per scan (17 → 16 on S1; `docs/dev/bench.md`
run D).

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

## Restore and flag (Phase 7)

Two more one-method seams, on the same terms as `accept`: the client hands over what it
rendered, the engine runs the op and a rescan in one critical section, and the answer
carries the post-write pile.

```rust
pub enum RestoreRequest {
    Hunk { rendered: Rendered, hunks: Vec<Hunk>, index: usize },
    File(Rendered),        // `rendered.oid == None` is the deletion form: put the file back
}
pub struct Restored { pub outcome: Outcome, pub seq: u64, pub pile: Pile }
pub struct RenderedHunk { pub hunk: FlagHunk, pub of: usize }
pub struct Flagged { pub outcome: Outcome, pub export: String, pub seq: u64, pub pile: Pile }

impl Engine {
    pub fn restore(&mut self, root: &Path, req: RestoreRequest) -> Result<Restored, EngineError>;
    pub fn flag(&mut self, root: &Path, path: &[u8], note: &str, hunk: Option<RenderedHunk>)
        -> Result<Flagged, EngineError>;
    pub fn unflag(&mut self, root: &Path, path: &[u8]) -> Result<Flagged, EngineError>;
}
```

Each has a `*_with(.., &dyn FaultInjector)` twin for the E1 seam, exactly as `accept_with`
does; production passes `NoFault`. A refusal is data (`Outcome::refused`) and still `Ok`;
`Err` is storage failure only, and the failure path reloads the ledger from disk the same
way accept's does.

### Restore writes the working tree, never the ledger

`Restored.outcome.written` is always `false`. A restore's whole effect is on disk: it puts
one hunk or one file back to the baseline the ledger already describes, so there is nothing
to record. That also means **restore is not undo-of-accept** — an accepted file is blessed
in the ledger and no longer pending, and `u` on a row it no longer shows is not a gesture
the TUI can make.

**Restore hunk `k` is `baseline ⊕ every content hunk but k`.** No reverse-apply and no
second diff implementation: while the live CAS holds, the file's current bytes *are*
`baseline ⊕ all hunks`, so dropping `k` from that selection is exactly "undo hunk k". Four
guards stand between the request and the write, in this order:

1. **The entry CAS** (`cas_live`) — the file is still the row that was rendered.
2. **The round-trip guard** (verifier F2). Before any temp file exists, the baseline blob is
   materialised through the store's own eol conversion and compared with the bytes on disk.
   If they differ — a `text=auto` root whose worktree holds CRLF that git would rewrite —
   the restore is refused (`Refused::NotRoundTrippable`, `<path>: eol conversion is not
   round-trippable; not restored`) rather than silently rewriting the user's line endings.
   Its own variant, not an `Unhashable` reason: the file hashed perfectly well — hashing it
   is how the guard knows — and the status line should not say otherwise (verifier (b) F7). Restore never normalises behind the
   user's back.
3. **The hunk CAS** (verifier F3). The file not having moved is not enough: `hunks::expand`
   truncates at a line cap, so an expanded collapsed row can hand over a *partial* hunk
   list, and `baseline ⊕ a fragment` would discard every edit the cap dropped while both
   file-level CASes passed. Reassembling the whole list and hashing it is an exact test — it
   equals the current oid precisely when nothing is missing — and a mismatch is
   `Refused::Incomplete`.
4. **The second CAS**, at the rename, as every other write has.

Two shapes are special-cased. The synthetic **mode hunk** is never in the selection
(`apply_hunks` would splice its literal `mode 100755` bytes in at offset 0, design review
F1): restoring it moves the mode alone, writing no bytes at all, and on a root that ignores
the executable bit it does nothing. An **added file's** single content hunk *is* the file
(verifier F4), so "everything but hunk 0" would write a zero-byte file and leave the row
pending; that case takes `restore_file`'s removal path instead.

**An absent baseline removes the file.** `empty_baseline_means_absent()` is
`ledger.seen_tree.is_some()` (decision 4 / verifier F17): a ledger that has a seen tree
records absence as the empty tree entry, so an empty baseline there means "this file was not
in the baseline" and restoring it deletes the file. Without a seen tree the same value means
"empty file", and restoring writes zero bytes. `restore_hunk` follows the same predicate as
`restore_file`, so the two never disagree about one row.

The refusal vocabulary is shared with accept and reads with the verb the caller passes —
`Refused::message("restored")`: `Moved`, `BaselineMoved`, `Unhashable`, `StillPresent` (the
deletion form: the file is back on disk already), `NonUtf8Path`, `NoSuchHunk`, `Conflicted`,
`Incomplete`.

### Flag appends, unflag clears the path

`Ops::flag` **appends** to `overrides[path].flags` and never touches `blob`: one review
raises several questions about one file, and the second must not eat the first. `unflag`
clears every flag on the path — Phase 7's UI has no per-flag removal, so "unflag" is the
undo for the whole path — and an override left with nothing else is removed. Both write the
ledger (`written: true`) through the same staged-then-commit path as an accept, and both
rescan afterwards: the rescan is what puts the new `⚑` on the row the UI is about to draw.

A flag is stored as `Flag { note, created_at, hunk: Option<FlagHunk>, summary:
Option<FlagSummary> }` with `FlagHunk { index, header, text }` — the hunk as it was
**rendered**, not a pointer into a diff that will have moved by the time anyone reads it —
and `FlagSummary { hunks, added, deleted }`, the whole-file counterpart (Amendment v1.8).
Both are additive and optional: a ledger written before v1.8 loads unchanged and
`SCHEMA_VERSION` stays `"1.1"`. The two are mutually exclusive by construction — `Ops::flag`
drops a summary offered beside a hunk — so a flag is a hunk flag or a whole-file one, never
a thing that claims to be both. Every write stamps the version. The JSON carries both
`flags` (the 1.1 list, hunks and summaries included) and `flag` (the 1.0 mirror of
`flags[0]`, **without** its `hunk` or its `summary`); `flag` is written as `null`, never
omitted, when there are no flags.

`of` — the `m` in `hunk n of m` — travels **from the caller** in `RenderedHunk` (verifier
F5). Deriving it from the flag's own rescan meant the header and text came from the screen
while the total came from the file as it is now, which produced shapes like `hunk 2 of 1`
when an agent rewrote the file between the render and the keystroke. The caller has the
number that was true when the user looked, and that is the only one the export may name.

### The export

`Flagged.export` is the paste-ready message, rendered by `flags::export` — in the engine
because only the engine has the flag's `created_at`. It is byte-frozen by
`crates/lastcall/tests/golden/flag_export.md`:

````text
lastcall flag · <root basename> · <root-relative path> · hunk 2 of 3 · 2026-09-05T18:04:00Z
note: why is this unwrap safe?

```diff
@@ -10,7 +10,8 @@
 context
-old
+new
```
````

A **whole-file** flag says `whole file` where a hunk flag says `hunk n of m`, carries a
summary line in the hunk block's place, and has no diff block at all (Amendment v1.8,
ruling P4):

```text
lastcall flag · alpha · src/tui/render.rs · whole file · 2026-09-05T18:04:00Z
3 hunks · +12 −4
note: the whole rewrite needs another look
```

The summary is `Flag.summary` (`FlagSummary { hunks, added, deleted }`), an **optional**
1.1 field: it is captured from the row as it was rendered when `m` was pressed, for the
same reason `of` is (F14), never from a rescan an agent may have invalidated. A flag
written before v1.8 has none and prints no summary line — the `whole file` segment is
unconditional, the line is not. A hunk flag never prints one: `Ops::flag` drops a summary
offered beside a hunk rather than write a flag that claims to be both. The TUI also sends
**no** summary for a collapsed row nobody expanded (verifier (a) F2): there are no hunks
the scan counted, and on a `Binary` row the `+a −d` are not line counts of a diff — an
absent line is honest where `0 hunks · +1 −1` would be a claim about the file
(`app_flag_on_a_collapsed_row_carries_no_summary`).

An `unflag`, or a refusal, leaves `export` empty.

Two rules make it safe to paste into a live terminal:

- **Control bytes render in caret form** (F13, F9, decision 10) — in *every* rendered field:
  root, path, timestamp, attribution, note, hunk header and hunk text. The export goes
  through bracketed-paste markers and the *application* decides where the paste ends, so a
  single `\x1b[201~` in the payload would close it early and let the rest arrive as
  keystrokes. C0 below `0x20` other than `\n` and `\t` becomes `^X` (`^[` for ESC), DEL
  becomes `^?`, and C1 `U+0080..=U+009F` becomes the caret form of its ESC equivalent (`^[[`
  for CSI) — a terminal in UTF-8 mode reads a raw `\u{9b}` as CSI, so C0 alone was not the
  whole hazard.
- **The fence is as long as it needs to be** (D1): one backtick longer than the longest
  leading run any line inside it starts with, minimum three. A diff line that is exactly
  ` ``` ` would otherwise close the block and spill the rest of the hunk into prose.

`ExportContext.attribution` (`last touched by <agent> · session <id>`) is still always
`None`. Phase 7 said "Phase 8 provides it"; Phase 8's deliverable list does not contain it,
so the field is carried unwritten and the line is never printed. Whichever phase adds the
herdr attribution to a flag owns it.

## Editing (Phase 8)

Phase 8 adds the third thing a reviewer does with a hunk: change it. Two engine entry
points serve it — `Engine::read_rendered` hands the TUI a file's bytes, and
`Engine::save` writes an edited buffer back — plus `Hunk::editor_line`, which is the
line both editors open at. `SCHEMA_VERSION` is unchanged: a save records an **override**,
the same ledger shape an accept records, so nothing on disk grew a field.

```rust
pub struct SaveRequest { pub rendered: Rendered, pub bytes: Vec<u8> }
pub struct Saved { pub outcome: Outcome, pub seq: u64, pub pile: Pile }

impl Engine {
    pub fn read_rendered(&self, root: &Path, rendered: &Rendered) -> Result<Vec<u8>, Refused>;
    pub fn save(&mut self, root: &Path, req: SaveRequest) -> Result<Saved, EngineError>;
}
impl Hunk { pub fn editor_line(&self) -> usize; }
```

`save_with(.., &dyn FaultInjector)` is the E1 twin, as for accept and restore. Unlike a
restore, `Saved.outcome.written` is `true`: a save writes the working tree *and* the
override.

**The write seam did not widen.** `Ops::save_file` reaches the working tree through
`restore::write_bytes`, so `restore.rs` is still the only file in the engine that opens a
path under a root for writing, and the gate grep says so:
`rg 'openat|renameat|OpenOptions|File::create|fs::write' crates/lastcall-engine/src --glob '!*test*'`
— every hit outside `restore.rs` writes under the **state dir** (`ledger.rs`, `index.rs`,
`store.rs`, `roots.rs`, `flags.rs`'s golden writer, `engine.rs`); `ops.rs`'s two hits are
inside its own `#[cfg(test)] mod tests`, which the glob does not exclude because the module
is in the file. A new hit under a root anywhere else is a verifier failure (kickoff
Boundaries, F2).

### `read_rendered` — the bytes the inline editor gets

`read_rendered(root, rendered)` is read-only: no write, no temp file, and **no ledger lock**,
so it can run on the blocking pool while the UI keeps drawing. It refuses the two rows a
save refuses outright (a deletion — "the file is gone"; a symlink — "not a regular file"),
CASes against the row on screen, reads the file, and hashes the fresh read through
`hash_bytes_as` to compare with `rendered.oid`: a mismatch is the agent that wrote between
the CAS and the read, and comes back as `Refused::Moved`. Then two limits the editor needs
and the save does not: over `collapse_size_bytes` and anything holding a NUL or invalid
UTF-8 are `NotEditable`, because a `TextBuf` is text. `$EDITOR` (`shift-i`) has neither
limit — it never loads the file into lastcall — which is why the TUI's refusal for the
inline editor names the other key (`use shift-i: <why>`).
Tests: `engine_read_rendered_opens_text_and_refuses_binary_and_oversize`,
`engine_read_rendered_refuses_a_file_that_moved_since_it_was_rendered`.

### `save_file` — the order is the rule

`Ops::save_file` is the **second** operation that writes the user's working tree, and the
only new one. It shares `restore::write_bytes` with the restore, so there is still exactly
one function in the crate that opens a path under a root for writing. Its order:

1. **Row shape.** `rendered.oid.is_none()` (a deletion) and `Mode::Symlink` are
   `NotEditable`: there is no file to write into, and a symlink's "content" is its target,
   so writing bytes at it either follows the link or replaces it — neither is an edit of the
   row on screen. (`ops_save_file_refuses_a_symlink_and_a_deletion`.)
2. **`restore_preflight`** — the restore's own preflight verbatim: a non-UTF-8 path key, a
   conflicted index entry (`Refused::Conflicted`), a parent chain that changed under us
   (`check_parent_chain`), and a `filter=` attribute on the path, which lastcall will not
   write through.
3. **The entry CAS** (`cas_live`), the same live compare-and-swap a restore takes. Unlike
   accept — which has no live CAS on purpose (Amendment A3), because it never touches the
   working tree — a save *writes*, so the CAS is the entire guard. **Do not harmonise the
   two.**
4. **The oid, before anything is written.** `Store::hash_bytes_as(rel, bytes)` runs
   `git hash-object -w --path=<rel> --stdin` under the same `GIT_DIR`/`GIT_WORK_TREE`/cwd a
   scan uses, so the path's `.gitattributes` — `text=auto`, `eol`, a clean filter — act
   exactly as they will on the scanned file, and the answer is *the oid the next scan will
   compute*. `--path` is the whole difference from `hash_bytes`, which passes none because
   its input is already-canonical blob content.
   The order is design review F1 and is easy to get wrong: `write_bytes`'s `before_rename`
   hook takes no arguments and cannot reach the temp file, and a fresh read *after* the
   rename would hash whatever an agent wrote in the rename-to-ledger window and bless it.
   Recording the oid of the bytes we wrote means such a write is **pending** at the rescan,
   which is invariant 2's direction.
   `-w` writes the object, so a save that is then refused leaves an **orphan blob** in the
   store (verifier (a) F6). Deliberate, and the same class of orphan a scan's `hash_path -w`
   leaves for content nobody accepts: the store is never gc'd (§11), the object is small,
   and the alternative is the window F1 closed. Do not "fix" it into a post-rename read.
5. **The write**, through `restore_write` → `restore::write_bytes`, with the **second CAS**
   in `before_rename` — so, as in a restore, the file is checked twice. The bytes go down
   verbatim (the caller read the worktree file, CRLF and all, so what comes back is what the
   user saw) with the **live** mode, so the executable bit survives and the override's mode
   matches what the next scan will `lstat`.
   (`ops_save_file_keeps_the_executable_bit`, `scenario_d3_save_of_a_crlf_text_auto_file_round_trips`.)
6. **The ledger, after the file.** Only once the bytes are on disk does `set_override` record
   `(oid, mode)` and `commit` write the ledger. A crash in that window leaves the new bytes
   and the old ledger — the edit is pending, nothing the user typed is lost, and that is the
   fail-open direction. (`ops_save_file_that_dies_before_the_ledger_shows_the_edit_pending`;
   a failure *before* the rename leaves no trace at all —
   `ops_save_file_that_fails_before_the_rename_leaves_no_trace`.)

The baseline is never touched: a save advances the **override**, which is what "the user is
never asked to review their own just-typed change" means (§6.3, invariant 8). The proof that
it holds is the pile, not the ledger: `Engine::save` runs the op and the rescan in one
critical section like `accept_with`, and a clean save leaves the row **gone**
(`ops_save_file_is_cas_and_the_rescan_shows_zero_pending`, and the proptest
`ops_save_then_scan_pends_nothing_for_any_bytes`, whose generator mixes CRLF, a lone CR, a
missing trailing newline, tabs, NUL and non-ASCII — 64 cases in prepush). The rescan is also
the §11 mitigation for the hash-then-rename window, exactly as for a restore, and the
orphaned open descriptor residual is inherited unchanged.

An `Err` out of the op — the ledger's failure, not the file's — drops the staged override
and re-reads the ledger from disk, so the saved bytes show up as *pending* at the next scan.
That is the honest answer when the file is written and the record of it is not. The refusal
vocabulary is the shared one, read with the verb `"saved"` — `Moved`, `Unhashable`,
`Conflicted`, `NonUtf8Path` — plus one variant Phase 8 adds, `NotEditable { path, why }`,
which carries its own reason and spells its own sentence (`<path>: <why>; not saved`)
because "not saved" is the only verb it can ever take.

### `editor_line` — where both editors open

`Hunk::editor_line()` is the one-based line in the **new** file that a hunk's first
non-context line sits on: it walks the hunk's leading context, so a hunk with three context
lines before the change opens on the change and not on the `@@` header. A pure insertion
lands on its first inserted line; a pure deletion lands on the line *after* the removed run,
because the new file has nothing else to point at, and that can be one past the end — both
callers clamp (`TextBuf::open` to the buffer, and every `$EDITOR` in the basename table
clamps a `+N` past the end to the last line). The synthetic mode hunk answers 1 and is never
an editor target: there is no text in it. (`hunk_editor_line_skips_leading_context`.)

### The blessing on `$EDITOR` return, and its residual

`shift-i` hands the file to the user's own editor, which writes on its own account —
lastcall never writes around it, and there is no CAS it could take over bytes it did not
produce. So the return path is a *question*, not a write: the loop re-hashes the path
(`Effect::EditorReturned`) and `App::editor_returned` decides.

| what the file is on return | what happens |
|---|---|
| the same oid **and** mode as the row | `no change`; nothing is asked |
| gone | `<path>: deleted on return; left pending` |
| unhashable (fifo, typechange, `EACCES`) | `<path>: <why> on return; left pending` |
| a symlink now | `<path>: not a regular file on return; left pending` |
| changed, and a confirm/note/picker is already open | `<path>: changed on return; left pending` (verifier (a) F1) |
| changed, and the screen is free | a confirm: bless the file at what the editor left |

**The residual is deliberate.** "Left pending" is not a lost edit: the row keeps whatever
the editor wrote and is reviewed like any other pending row, which is the fail-open
direction and the only safe one — an editor return arrives on a channel, and replacing a
question the user is reading with a different one would make their next `y` answer something
they never saw. A blessing that *is* taken is an ordinary accept of the live row
(`AcceptScope::Bless`), so it goes through the same ledger path, the same rescan and the same
§6.7 advance as `A`.

Two windows nothing closes, and nothing can: between the editor's save and lastcall's hash,
and between that hash and the confirm's accept, another writer can change the file. The
second is covered — the accept is CAS'd against the row the confirm named, so it refuses —
and the first shows up as a normal pending row at the next scan. That is the whole guarantee
$EDITOR admits of.

## `status --json` schema (`status_version: 1`)

```json
{
  "status_version": 1,
  "state_dir": "/abs/state-dir",
  "notices": ["engine-level notices (config resolution, ad-hoc cwd, moved roots)"],
  "roots": [
    {
      "root": "/abs/path", "kind": "git | draft", "parent": "/abs/parent-dir",
      "store": "<repo-hash>", "ledger_written_at": "2026-09-07T19:21:35Z | null",
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

`state_dir`, `store` and `ledger_written_at` (additive, Phase 9a / Amendment v1.9;
`status_version` stays 1) name **which store this run read**. `state_dir` is the resolved
state dir (`LASTCALL_STATE_DIR` → `$XDG_STATE_HOME/lastcall` → `~/.local/state/lastcall`,
`config::state_dir`), always absolute: a relative `LASTCALL_STATE_DIR` is joined onto the
launch cwd — where the store already lands — and never canonicalised, so no symlink is
resolved and a store that does not exist yet still names itself (verifier (a) F3;
`config_relative_state_dir_is_absolutised_against_the_cwd`); `store` is the root's
`<repo-hash>` directory name under
`<state_dir>/roots/<parent-hash>/repos/`, so `jq -r '.roots[] | "\(.store) \(.root)"'`
gives the `cd` target for the walkthrough above; `ledger_written_at` is `ledger.json`'s
mtime as ISO-8601 UTC with second precision, `null` when no ledger has been written yet
(the file was removed, or the root has not had its first sight). They exist because two
runs that disagree about a repo are almost always two runs over two state dirs, and until
v1.9 the report said nothing about which one it read (§10 2026-09-07; the closed §11
entry). `lastcall status`'s human form prints `state dir: <path>` as its first line for the
same reason; `lastcall config`'s `state_dir:` line is unchanged.

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
edge (750 ms **with a 3 s starvation cap**), and two polling backstops remain (HEAD every
10 s, root discovery every 30 s). The cap is Amendment v1.6: the trailing edge alone
postpones a root's scan for as long as a writer keeps writing, so a scan is scheduled
`debounce` after the latest event **but never later than `debounce_max` after the first
event of the burst** (`watcher::schedule`; a `min` of the two deadlines). Both are per
root, both are hardcoded — `--poll N` moves the two backstops and nothing else
(`poll_timings_none_keeps_defaults_and_zero_clamps_to_one_second` asserts the debounce pair
survives it), and neither has a config key (Q6). A scan the loop ran ends the burst window
and the next event opens a fresh one; a scan the loop did **not** initiate (the TUI's own
`Effect::Refresh`, or the rescan an accept leaves behind) does not, which costs at most one
redundant scan per window.
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

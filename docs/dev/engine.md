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
    index.tmp        # scratch index for accept-all / compaction folds
    lock             # flock for ledger writes (20 × 50 ms, then the op errors); every write
                     # reloads the on-disk ledger under it and replays the in-memory change,
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
`ledger.json.tmp` found at open (a crash between the two steps, E1) is removed with a notice.
An unreadable ledger is moved aside to `ledger.json.unreadable-<secs>-<n>` and the root
opens with `seen_tree = null` (every path pending); that ledger is written at once so the next
open finds it — it is *not* first sight, because first sight at the current HEAD would hide
everything committed since.

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
`GIT_CEILING_DIRECTORIES` from the child after the overlay.

| runner | cwd | env | may write? | used for |
|---|---|---|---|---|
| `StoreGit` | the root | `GIT_DIR=<store>`, `GIT_WORK_TREE=<root>`, `GIT_INDEX_FILE=<index>` explicit on every call | yes, to **our** store and index only | `hash-object -w --stdin-paths`, `read-tree`, `write-tree`, `ls-tree`, `cat-file --batch-check`, `diff-files`, `ls-files --others`, `update-index --refresh` |
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

1. Refresh the private index (`update-index --refresh`); a held `index.lock` is retried 3 × 50 ms, then the scan proceeds unrefreshed (over-report at worst).
2. Candidates = `diff-files` paths ∪ `ls-files --others` files ∪ every override path ∪ (case-insensitive roots) seen-tree names absent byte-exactly from their directory.
3. Minus paths tagged skip-worktree in the **user's** index *that are absent from the worktree* (D6: a sparse cone; a present one is a real edit and stays); minus `others` entries under a nested repo (D9) or a sibling draft root. `diff-files`/override/case paths are never excluded.
4. `current` per path = `lstat` → `Absent` | `Unhashable(reason)` | `{oid, mode}`; all hashed in one `hash-object -w --stdin-paths` call from the root (so `text=auto` and clean filters apply). `Unhashable` is always a row.
5. `baseline` per path from the ledger (override blob → override null → seen tree → empty); a missing override blob or an unparsable override falls to the tree with a notice (E2).
6. Row iff baseline ≠ current by oid or mode (D1); on `core.filemode=false` roots the executable bit is normalized away on both sides, so a mode-only row cannot appear there.
7. Hunks from a byte diff of the two blobs (`hunks.rs`); binary (NUL in the first 8000 bytes) or ≥ `collapse_size_bytes` or matching `collapsed_globs` → collapsed (D7/D8).
8. Conflict state from the user's `ls-files -u` (C4); the override's flag; rename pairing (D5) — presentation only, the ledger stores delete + add.
9. Annotation (`upstream.rs`): heads = HEAD (+ `MERGE_HEAD`); range `seen_head..head` or from the merge-base; commits reachable from a remote ref by someone else are upstream; a pending path whose content equals a head's blob is `upstream`, touched upstream but different is `mixed`, otherwise plain. No merge-base, detached with no upstream, shallow, or no remotes → nothing annotated (C7).
10. Notices from every step ride along in `pile.notices`; nothing after step 6 removes a row.

## The fail-open ladder

Every rung shows *more* than the truth, never less, and says why in a notice:

| condition | behaviour |
|---|---|
| no ledger | first sight: seen tree = `HEAD^{tree}` (`null` before the first commit); drafts follow `draft_initial` (`seen` snapshots the dir, `pending` anchors to no tree) |
| a sibling ledger whose recorded root is gone (the repo was moved, E4) | first sight for the new path, a notice naming the old root; old state is kept |
| unreadable ledger / unknown schema major | moved aside, `seen_tree = null`, every path pending |
| stale `ledger.json.tmp` | removed at open, notice (E1) |
| seen tree not resolvable in the store (user ran `git gc`) | treated as `null`, notice |
| override blob missing (E3) or override unparsable (E2) | that path resolves to the seen-tree entry |
| `index` / `index.tree` missing, mismatched, or unreadable (empty, garbage, truncated) | reseeded from the seen tree; the pile is identical |
| `index.lock` held by another process | scan runs unrefreshed |
| a path that cannot be hashed (unreadable, a socket, `git-lfs` missing) | an `Unhashable` row |
| accept whose rendered oid/mode/baseline no longer matches the live file | refused, nothing written; the next scan shows the new state (A5/A6) |
| ledger lock busy after 20 × 50 ms | the op errors; nothing is written unlocked |

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
      "groups": [{"kind": "upstream", "paths": ["u1"]}],
      "notices": ["root-level and scan notices"]
    }
  ]
}
```

Roots are sorted by path bytes, rows by path bytes; paths are lossy UTF-8 (a non-UTF-8 path
renders with U+FFFD but is still a row). The committed example is
`crates/lastcall/tests/golden/status_multi_repo.json` (two repos and a draft dir, produced by
`lastcall_testkit::fixture_parent`; `just golden-update` rewrites it).

## Watching

`Engine::run` (`watcher.rs`) puts one recursive `notify` watch on each **parent dir** (plus
any root not under one, and any git dir outside them — a linked worktree's common dir). The
watches are installed on a blocking task while the initial scans run: on macOS every `watch`
call re-registers the FSEvents stream, which took ~1.8 s per call on the development machine,
and the first pile must not wait for that. When the watch goes live every root is scanned
and inspected once more (nothing from the gap is missed) and a `watching <dirs> (N roots)`
notice is emitted; the rescan backstop counts from there. Git-dir events are filtered to an
allowlist (`HEAD`, `index`, `ORIG_HEAD`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
`refs/`, `rebase-merge/`, `rebase-apply/`, `logs/`), worktree events are debounced on the
trailing edge (750 ms), and two polling backstops remain (HEAD every 10 s, root discovery
every 30 s). Events only *schedule* work: every scan, head inspection and rescan runs on
`spawn_blocking` under the engine's mutex, and the result is published as an `EngineEvent`
(`Pile`, `Head` with its transition notice, `RootsChanged`, `Notice`). `ignore_globs` scope
the watcher only — an ignored path never wakes a scan, but the next scan still shows the
tracked edit. `lastcall watch [--json] [--exit-after N]` prints one line per event;
`just probe-watch` demonstrates the B1 notice (`committed on main (1 commit)`) arriving while
the pending row stays.

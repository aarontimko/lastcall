# lastcall: an agent-agnostic review ledger for the terminal

*(Named 2026-09-01 — the last call before it ships. Earlier drafts used "ledgr" and "accptr"; see §10.)*

**Status:** **STAMPED at G0 on 2026-09-01 — §5 and §6 are frozen at v1.0** (sponsor ruling, verbatim: "if you don't have any questions, you can take my stamp and apply G0"; every §9 default adopted as recommended, recorded in §10). This document is the program's **design corpus and roadmap** in one file, structured for the phased-program method: North Star and product principles first, then program governance, then the two frozen contracts (the herdr surface we consume, and our own contracts), then testing strategy and the phase roadmap with every gate checklist written in advance.

**Freezing rule:** at G0 the human sponsor stamps this document. On stamping, §5 (herdr surface contract) and §6 (our contracts) freeze at **v1.0** and change only through versioned amendments (v1.1, v1.2, ...) recorded in the Amendments log, made only when implementation surfaces something unknowable at design time. Gate checklists (§8) change only by deliberate amendment per §3.4. Open questions for G0 are collected in §9.

**Verification provenance:** every claim in §5 was verified on 2026-09-01 against `herdrdev/herdr` @ commit `5158ada` (master, 2026-08-31) — **which is 55 commits past the v0.8.2 release tag and reports protocol 21, while the published v0.8.2 binaries report protocol 20** (found by the Phase 1 worker's real-binary test; see Amendment v1.1 for the two behavioral differences that matter to us) — by reading both the published machine-readable schema (`docs/next/api/herdr-api.schema.json`, retrievable offline via `herdr api schema --json`) and the Rust source (`src/api/`, `src/app/api*`, `src/session.rs`, `tests/api_ping.rs`). Two independent research passes were followed by a direct fact-check pass that re-read every load-bearing claim in the source first-hand (subscription enum, event envelopes, emit sites, seen-flip sites, event-hub ring, server stream loop, notification handler, metadata patch/seq logic, plugin pane handler, env-var injection, CLI schema command). Source citations below are file:line in that clone. Where this draft's predecessor was wrong about herdr, the correction is marked **[corrected]**. §6's git contracts were verified executably on 2026-09-01 by the scenario harness (§7.4: 46/46 assertions against real git), and §6.8's agent-hook facts against each agent's documentation the same day.

---

## 1. North Star

**One sentence:** a terminal-native pane that watches every repo under your working directory, shows you exactly what changed since you last looked, and lets you burn through it hunk by hunk with yes/no clicks, regardless of which AI agent (or human) made the edits.

### 1.1 The problem

Agentic development inverted the review bottleneck. Agents produce diffs faster than humans read them, and the standard tools for reading them fall into two camps that both fail the terminal-first, multi-agent workflow:

1. **`git diff` and friends** have the complete data but no memory. A diff is a static printout, like a Word document with tracked changes: there is no way to mark "I have seen this file, move on," so on any pass after the first you are re-scanning content you already reviewed and relying on your own memory to know what is new. Committing clears the view entirely, which means an agent that commits before you look has effectively hidden its work behind `git show`.
2. **IDE review ledgers** (Cursor's changed-files list, VS Code Copilot's Keep/Undo) solve the memory problem beautifully: a live list of touched files with line counts, per-file dismiss, per-hunk walk-through. But they are bound to their own agent and their own editor. They track only edits made through their tools, they see one workspace at a time, and they do not exist in the terminal at all. Run Claude Code in a terminal pane and Cursor's ledger stays empty.

Anyone running multiple terminal agents (Claude Code, Codex CLI, and whatever ships next quarter) across multiple repos has no review surface at all. That is the gap.

### 1.2 The user and the workflow this serves

The primary user (and initial only user) is a senior engineer who:

- starts sessions from a **non-git parent directory** containing all cloned repos, and frequently works across several of them in one effort;
- lets agents run **freely, without per-edit approval**, and reviews **post-hoc** once an agent finishes (occasionally peeking mid-flight);
- runs at most **two dense agent efforts at a time** plus small spin-off tasks, using **git worktrees** when agents must share a repo;
- reviews by triage: accept the obviously-fine files wholesale, walk the uncertain ones hunk by hunk, and rather than reverting, **flag** the questionable hunk and discuss it with the agent;
- drafts prose (replies, research notes) in **gitignored directories** and wants the same review treatment for those files;
- lives in a multiplexed terminal (herdr) with agents on the left and wants this tool as a **pane on the right**.

### 1.3 Why not something else

| Alternative | Why it falls short |
|---|---|
| `git diff` / `git show` | No seen-state, no per-file dismissal, cleared by commits, blind to ignored files. |
| Cursor / Copilot ledgers | Agent-bound and editor-bound; empty for terminal agents; single workspace; not a terminal citizen. |
| Codex review pane | Git-shaped: requires a git repo, reflects repo state rather than a seen-baseline, invisible to ignored files. |
| lazygit | Closest existing UX (file list, hunk staging, discard). But review-by-staging overloads git's index semantics, it is one repo per invocation with no cross-repo rollup, and it cannot see ignored files. |
| Per-agent hooks (Claude Code PostToolUse etc.) | Works, but couples the tool to each agent's hook API. Every new agent or hook-schema change becomes maintenance. |
| drydock (`yetidevworks/drydock`) | Closest neighbor found (inspected 2026-09-01 @ v1.1.0, MIT, Rust/ratatui/notify — the same stack). A *fleet-status* dashboard: per-repo uncommitted/unpushed/unreleased counts across every repo you own. No seen-state, no baselines, no diff or hunk view, no accept/flag, no ignored-file drafts, no agent awareness — it answers "which repos have stuff," not "show me exactly what changed since I looked, and let me burn it down." Validates the multi-repo-TUI niche; complementary rather than competing. |

The design bet: **watch the filesystem and git state, not the agents.** Any tool that mutates a worktree is automatically supported, forever, with zero per-agent integration code.

### 1.4 Why the herdr integration is the value multiplier

herdr (the agent multiplexer this tool is designed to live inside) already solves the questions this tool would otherwise have to answer badly:

- **When is review time?** herdr's semantic agent state includes `done`, defined as "idle and not yet seen by the user" — verified to be exactly `(internal state Idle, seen flag false)` in source (`src/app/api_helpers.rs:96-107`). That is a purpose-built review trigger, delivered over a local socket.
- **Which repo does an agent belong to?** herdr workspaces carry worktree provenance (`repo_key`, `repo_name`, `repo_root`, `checkout_path`) and panes carry `cwd` / `foreground_cwd`.
- **When do repos appear and disappear?** `worktree.created` / `worktree.opened` / `worktree.removed` events announce worktree changes — **but only for operations performed through herdr itself** (its UI or its API; verified: all emit sites live in herdr's own worktree-operation handlers, and herdr has no filesystem watcher). An agent running `git worktree add` in its shell emits nothing. herdr events are therefore a fast-path accelerator for the watch set, not a replacement for our own periodic rescan (§6.5).

The integration is **additive, not required**. Standalone mode (any terminal: iTerm2, Ghostty, a bare tmux pane) runs the identical engine with filesystem watching and git state only; the herdr socket adds agent status, review flags, and dynamic worktree discovery when present. Detection is automatic: if `HERDR_ENV=1` / `HERDR_SOCKET_PATH` is set or the default socket exists, connect; otherwise run standalone.

### 1.5 Distribution and licensing **[corrected]**

Open source, published under the author's personal GitHub account.

The predecessor draft asserted herdr is AGPL-3.0 and derived a hard constraint from that. **This is wrong: herdr is Apache-2.0** (verified: `LICENSE` line 1 and `Cargo.toml:7 license = "Apache-2.0"` at v0.8.2). Consequences:

- The licensing constraint dissolves. Reading herdr source for architectural reference is unambiguously fine; adapting small amounts of code (e.g. the test-harness `JsonLineReader` pattern, §7.4) is fine with Apache-2.0 attribution (retain notices; note the borrowing in a `NOTICE`/README credit).
- Our license choice is unconstrained by herdr. **Decided at G0 (§10):** the Rust-conventional **MIT OR Apache-2.0 dual license**; `LICENSE-MIT` and `LICENSE-APACHE` land in Phase 1.
- herdr also has a **first-class plugin system** (verified, §5.11) whose `split`/`tab` placement panes are ordinary herdr panes. Packaging this tool as a herdr plugin is a designed-for later phase, not a v1 deliverable.

---

## 2. Product principles (invariants)

These are contract-level. Any implementation choice that violates one is a bug.

1. **The ledger is recomputable from scratch.** It records only what the user has seen (baselines and flags), never what changed. Pending is always computed fresh as `diff(baseline, current)`. Restart, crash, or missed watcher events cost nothing but a rescan.
2. **Uncertainty fails open to pending.** A missing blob, unreadable ledger entry, or ambiguous state re-flags content as unseen. The tool may over-show after a failure; it must never silently hide a change.
3. **Accept is metadata-only, and compare-and-swap.** Accepting a hunk or file writes to the ledger and blob store, never to the working file, and always targets the exact content hash the rendered diff was computed from — never a fresh read of the live file. Review is therefore race-free while agents are still writing: an agent write between render and click causes a refused accept and a re-render, not a silently blessed change.
4. **Restore is the only file-writing operation, and it is guarded.** Every restore is compare-and-swap: hash-checked against the content the rendered diff was computed from, refused with a re-render if the file moved underneath.
5. **HEAD movement alone never creates or destroys pending**, except via the ancestry rule (§6.4). Commits, in particular, do not clear the review queue.
6. **Every synthetic baseline has a defined death** (branch-scoped via commit keying, killed by history rewrite). No baseline is managed forever.
7. **Agent-agnostic by construction.** No per-agent hooks, no agent-specific code paths in the engine. herdr integration touches presentation and triggers only.
8. **The tool reviews worktree state, not authorship.** Human edits in the same worktree appear as pending, which is correct. Exception: edits made through the tool's own editor advance the baseline on save, so the user is never asked to review their own just-typed change.
9. **Event streams are hints; snapshots are truth.** All push-channel input (filesystem events, herdr events) only ever schedules recomputation or resync against an authoritative source (`git status`, a stat walk, `session.snapshot`, `pane.list`). No state the user relies on is ever derived solely from having observed an event. This is invariant 1 extended to the herdr client, and it is load-bearing because herdr's event delivery is verifiably lossy under load (§5.6).

---

## 3. Program governance (phased-program method)

### 3.1 Roles

- **Orchestrator** (main Claude session): writes each phase's kickoff spec from this corpus, launches and babysits workers, personally spot-verifies claims, presents rulings, merges worker branches, maintains docs and the handoff record. Never builds a phase itself.
- **Workers** (background subagents, `isolation: worktree`): build exactly one phase from one kickoff spec, in their own git worktree. Report with evidence (test counts, snapshot names, command output), not enthusiasm.
- **Verifiers** (fresh-context subagents): adversarial, milestone-triggered, hunting named failure classes (e.g. "accepted hunk resurrection", "weakened test pins", "restore without CAS check").
- **Human sponsor** (Aaron): rules on open questions, ratifies version bumps to frozen contracts, pushes every branch, merges every PR.

### 3.2 Frozen artifacts

| Artifact | Version | Freeze point | Change protocol |
|---|---|---|---|
| §5 herdr surface contract | v1.0 at G0 | G0 stamp | Amendment log; human ratifies |
| §6 our contracts (storage, ledger schema, operations, git state machine, UI contract) | v1.0 at G0 | G0 stamp | Amendment log; human ratifies. On-disk `schema_version` bumps require a read-migration. |
| §8 gate checklists | — | G0 stamp | Deliberate amendment only; an unmet gate item becomes a named entry obligation of a later phase, never waived silently |

Workers may **propose** contract changes (implemented as additive, no-op-when-absent), documented in their report as a proposed vN+1. Only the human ratifies; only then is this document edited.

### 3.3 Decision log and deferral ledger

- **Decision log** (§10): every G0/phase ruling recorded verbatim, dated, tagged `[OPEN]`/`[LEANING]`/`[DECIDED]`, supersedable — re-tagging with a pointer, never editing history.
- **Deferral ledger** (§11): deliberately accepted shortcuts, each with why-acceptable, what-hardening-looks-like, the trigger condition that makes fixing it mandatory, and where it was decided.

### 3.4 Gate amendments

A gate is a checklist, not a vibe. An item that genuinely cannot be met in its phase is amended deliberately: the roadmap entry is edited, the item becomes a **named entry obligation** of a specific later phase with its exact baseline evidence recorded, and the PR carrying the amendment says "approving this PR ratifies the gate amendment."

### 3.5 Scheduled circle-backs

- After Phase 4 (product moment): re-review the G0 rulings on ack semantics and accept-all confirmation against real usage.
- After Phase 5 (herdr in UI): re-verify §5 against the then-current herdr **release tag** (never master); the scheduled schema-diff job now lands in Phase 5 itself (amended 2026-09-01, §10), so the circle-back reads its first run rather than doing the diff by hand.
- Before Phase 9 (release): review the deferral ledger in full; derive the release checklist from it.

---

## 4. Platform, project requirements, environment contract

### 4.1 Language and key crates

- **Rust.** TUI via **Ratatui** (mouse events, scroll wheel, click targets) — herdr pins `ratatui = "0.30"`; match the same minor to keep herdr's test patterns transferable. File events via **`notify`** (FSEvents backend on macOS) — note this is our choice; herdr itself has **no filesystem-watch dependency at all** (verified: zero `notify` in its `Cargo.lock`), so nothing about watching can be imitated from herdr. Git via **shelling out to the system `git` binary** for v1 (battle-tested plumbing; a `gix` migration is a possible later optimization, not a v1 concern). PTY-level e2e via **`portable-pty`** — herdr pins `=0.9.0` and vendors it with two Windows-related patches; we use the crates.io release (we don't target Windows in v1, so the patches don't apply to us).
- **Crate layout:** engine as a library crate, TUI as a thin binary crate over it (workspace with `lastcall-engine` / `lastcall`), plus a `lastcall-testkit` crate for test-only support (mock herdr server, PTY spawn helpers, fixture-repo builders). The engine must be drivable entirely from tests with no terminal.
- **Toolchain (added at G0, 2026-09-01):** ratatui 0.30.2 declares MSRV 1.88 and herdr builds on a pinned `1.96.1`; the sponsor's machine had Homebrew's rust 1.68 (2023) and no rustup. The repo therefore commits a `rust-toolchain.toml` pinning a stable channel (`1.98.0` at Phase 1, components `clippy` + `rustfmt`), installed via **rustup** locally and via `dtolnay/rust-toolchain` in CI reading the same file — one pin, both places. Edition 2024.

### 4.2 Platforms

macOS is the primary and only fully supported target. Linux should work by construction (notify/Ratatui are cross-platform) and is accepted best-effort; CI runs Linux because GitHub runners make it free, but Linux-specific polish is not a goal. Windows is explicitly out of scope for v1; nothing should gratuitously preclude it (avoid hardcoded `/` assumptions where cheap), but no effort is spent on it. Two herdr facts that reinforce this: `foreground_cwd` is unconditionally `None` on Windows (`src/pane.rs:3024-3027`), and herdr's own socket-API test harness is unix-only.

### 4.3 Repo and conventions

- Public GitHub repo under the author's account. Layout follows house conventions: `src/`, `scripts/`, `tests/`, concise `README.md` with the bulk of documentation in `docs/`, plus an `AGENTS.md` stub-index per the progressive-disclosure convention.
- Conventional commits, first line under 60–80 characters; branches `feat/...`, `fix/...`, `chore/...`, `docs/...`; PRs to `main`. One phase = one branch (`feat/phaseN-<slug>`) = one PR, carrying its kickoff spec as an early commit.
- **Build/test entry points:** a `justfile` with at minimum `just build`, `just lint`, `just test-unit`, `just test-integration`, `just test-e2e`. Unit tests are in-module (`#[cfg(test)]`, run by `cargo test --lib --bins`); integration and e2e test files follow `test_integration_*` / `test_e2e_*` naming (Phase 1 kickoff review, F17: a `test_unit_*.rs` file would not be counted by the unit command). These exact invocations are the canonical suite commands cited in every kickoff spec and every gate.

### 4.4 Environment contract (adapted from the phased-program defaults)

- **No docker stacks needed** — this is a CLI/TUI program. The isolation unit is the git worktree per worker plus per-test temp dirs.
- **Sacred and untouchable:** the user's real herdr session and config (`~/.config/herdr/`). No test, worker, or CI job ever points at the real socket or mutates real herdr config. All herdr-facing tests use the isolation recipe in §5.10 (private `XDG_CONFIG_HOME`, explicit `HERDR_SOCKET_PATH` in a temp dir, `HERDR_ENV` removed). Equally sacred: the author's other repos under the dev parent directory — integration tests operate only on fixture repos they created.
- **Push hook:** agent-initiated `git push` is blocked; every branch crosses the network by the human's hand. The orchestrator hands over the exact push command at each phase close.
- **Permission allowlist:** committed in `.claude/settings.json` at Phase 1, covering exactly the `just` targets, `cargo` forms, and `git` read forms workers live in. New tooling funnels into `just` targets rather than widening the allowlist.
- **Test-floor ratchet:** every kickoff spec from Phase 2 onward states the unit-suite floor as a number; the floor only ratchets upward. Weakened or deleted tests are a named verifier failure class.
- **Spend posture:** all phases are construction phases with no LLM API calls in the product; expected worker spend is the well-mocked-construction band (≈$0.20–$0.90/phase). Tripwire $5 per phase — a stop-loss, not a budget.
- **herdr version pinning for tests:** CI installs a **pinned herdr release** (recorded in the repo, starting at v0.8.2) for integration tests; the separate scheduled compatibility job (Phase 9) tracks latest. The pin lives in one place (`herdr.version` in the `justfile`), and `just herdr-fetch` downloads the matching official release asset (`herdr-macos-aarch64`, `herdr-macos-x86_64`, `herdr-linux-x86_64`, `herdr-linux-aarch64` from `github.com/herdrdev/herdr/releases`) into `target/herdr/<version>/herdr` via `gh release download`; tests locate the binary through `LASTCALL_TEST_HERDR_BIN` (set by the `just` target) and **skip with a visible reason** when it is unset — never fall back to a `herdr` on `PATH`, which could be the user's live install. Note (2026-09-01): no herdr binary is installed on the sponsor's machine at all, so the `[sponsor]` items in §8 need a real install first.

### 4.5 CI/CD

GitHub Actions:

- **PR workflow:** lint + unit + integration on macOS and Linux runners. Note from herdr's own suite: its server-spawn test helper is gated `#[cfg(target_os = "linux")]` (`tests/api_ping.rs:106`) and several event helpers are compiled out on macOS — timing-sensitive PTY/socket tests are treated as Linux-first upstream. herdr publishes `x86_64`/`aarch64-unknown-linux-musl` release binaries, so pinning a herdr version in Linux CI is straightforward. Budget for the same: e2e/PTY tier runs on Linux in CI, on macOS locally, with macOS CI e2e as best-effort non-blocking.
- **Release workflow:** signed release binaries on tag push, GitHub Releases (Homebrew tap is a later phase).
- **Scheduled herdr-compat workflow** (Phase 5; moved from Phase 9 on 2026-09-01, §10): install latest herdr release, run the real-server integration subset, diff `herdr api schema --json` against our consumed-surface fixture (§5.9), open an issue on drift.

### 4.6 herdr support policy

The repo declares a tested herdr version range (initially `>=0.8.2`, protocols **20 and 21** — Amendment v1.1). At startup the client calls `ping`, compares protocols, tolerates unknown fields everywhere, and degrades to standalone mode with a visible notice if the socket is absent or the protocol mismatches. Important: **the server never rejects a mismatched client** — `ping` always answers and no other request carries a client version. Version enforcement is entirely our side, imitating herdr's own CLI guard (`src/cli/protocol_guard.rs`): ping, compare `pong.protocol`, refuse politely on mismatch. An `unknown method` style error is treated as a version signal, not a bug.

---

## 5. Phase 0 contract, part A: the herdr surface we consume (v1.0 — frozen 2026-09-01)

Everything below is verified against herdr v0.8.2 source and schema (provenance in the header). The machine-readable schema is retrievable offline from any herdr binary via `herdr api schema --json` (it is `include_str!`-embedded at compile time, so it is byte-exact for that binary's version).

### 5.1 Transport and connection model

- Newline-delimited JSON over a Unix domain socket, file mode `0o600`. Default path `<config_dir>/herdr.sock` where `config_dir` is `$XDG_CONFIG_HOME/herdr` or `~/.config/herdr`; named sessions at `<config_dir>/sessions/<name>/herdr.sock`. **Debug-built herdr uses `herdr-dev` instead of `herdr`** as the app dir — relevant when testing against a locally built herdr.
- Socket resolution order (precedence logic at `src/session.rs:80-90`, path assembly at `:169-181`): explicit `--session` → `HERDR_SOCKET_PATH` (used verbatim) → `HERDR_SESSION=<name>` → default.
- **One request per connection.** The server reads exactly one request line, writes one response line, and closes (`src/api/server.rs:161-309`). There is no pipelining and no per-connection state. Request/response correlation via `id` exists but is trivial given one-shot connections. Long-lived methods own their connection for their lifetime: `events.subscribe`, `events.wait`, `agent.wait`, `agent.prompt` (with `wait`), `pane.wait_for_output`.
- Request envelope: `{"id": "<string>", "method": "<name>", "params": {...}}` — **`params` is required on every method, including `ping`** (send `{}`).
- Success: `{"id": ..., "result": {"type": "<snake_case discriminant>", ...}}`. Error: `{"id": ..., "error": {"code": "<string>", "message": "<string>"}}`. Distinguish by presence of `result` vs `error` (herdr's own client does exactly this).
- Error codes are **plain strings, not a schema enum**. Handle codes we care about by name (`pane_not_found`, `workspace_not_found`, `worktree_list_failed`, `invalid_params`, `timeout`, `agent_blocked`, `agent_not_found`, `server_unavailable`) and treat any unknown code as a generic failure. **[corrected]** The docs' own error example shows an error *code* `not_found` that does not exist in source; never pattern-match on it as an error code. (`not_found` does exist as a `reason` value of `pane.focus_direction`, which we don't use.)
- Server-side limits worth respecting: request line ≤ 1 MiB (over-limit drops the connection with no JSON error); 5 s server-side timeouts on initial read and app dispatch; 5 s write timeout on event streams (see §5.6).
- No auth or registration; any local process is a client.
- When our pane runs inside herdr, the process inherits `HERDR_ENV=1`, `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH` (path to the herdr binary — useful for shelling out portably), and — when launched as a managed pane — `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`, `HERDR_PANE_ID`. This is the standalone-vs-integrated detection signal and lets the pane self-locate. Additionally, a keybind-launched custom command receives `HERDR_ACTIVE_WORKSPACE_ID` / `HERDR_ACTIVE_TAB_ID` / `HERDR_ACTIVE_PANE_ID` / `HERDR_ACTIVE_PANE_CWD` and runs in the focused pane's cwd — the cleanest user setup for "press a key, get the review pane."

### 5.2 Versioning and compatibility posture

- Wire protocol is a single monotone u32. **[v1.1]** The released v0.8.2 binaries = **protocol 20** (`src/protocol/wire.rs:16` at tag `v0.8.2`); master after the release = 21 (unreleased at 2026-09-01). `ping` result: `{"type":"pong","version":"0.8.2","protocol":20,"capabilities":{...}|null}`. The client accepts `SUPPORTED_PROTOCOLS = [20, 21]`; the wire surface we consume is identical on both (the 20→21 schema delta adds `WorkspaceCloseParams.close_group` and `trust_repository` on worktree params, neither of which we use).
- herdr's entire published stability policy: protocol changes are release-reviewed; check `ping` before depending on new behavior; **handle unknown fields gracefully**. Verified structurally: no `deny_unknown_fields` anywhere in herdr's API layer — the server ignores unknown params and we must ignore unknown response/event fields. Accordingly, serde `deny_unknown_fields` is forbidden on all herdr-facing types in our codebase, and absence of any optional field means "not available," never an error.
- No documented deprecation policy exists; the scheduled schema-diff job (§4.5) is our early-warning system.
- All herdr-facing types live in one module, hand-checked against the schema, with our consumed subset captured as a committed fixture the compat job diffs against.

### 5.3 Bootstrap (documented pattern, adopted verbatim)

1. Open connection A, send `events.subscribe` with our subscription set, wait for the acknowledgement line `{"id":...,"result":{"type":"subscription_started"}}`, then buffer the stream. Note: if **any** subscription in the set fails to construct, the server sends one error and **closes the connection** — no partial subscriptions.
2. On connection B, call `session.snapshot`. Response `snapshot` contains exactly: `version`, `protocol`, `focused_workspace_id?`, `focused_tab_id?`, `focused_pane_id?`, `workspaces[]`, `tabs[]`, `panes[]`, `layouts[]`, `agents[]`. Worktree provenance rides on each `WorkspaceInfo.worktree`; **there is no top-level worktrees array** — full per-repo worktree enumeration is `worktree.list`.
3. Install the snapshot into the local cache. **Buffered events are not replayed as state mutations** — events carry no sequence numbers, so the client cannot tell which buffered events predate the snapshot, and replaying a stale `pane_updated` would regress fresher snapshot state (review finding F10). Per invariant 9, events only ever *schedule* a resync of the affected object; "replay" therefore reduces to "if anything was buffered, run one resync." Then continue streaming. (On master the server snapshots its event cursor before constructing subscriptions, so events during setup are not lost. **[v1.1]** On the released v0.8.2, lifecycle event subscriptions start at sequence 0 — `ActiveEventSubscription { last_sequence: 0 }` at the tag — so a new lifecycle stream **replays every event still in the 512-entry ring**, draining at one per subscription per 100 ms; per-pane status subscriptions already start at the current cursor on both. Consequence: for up to ~50 s after every connect or reconnect the lifecycle stream carries stale history. Invariant 9 absorbs it — replayed events only schedule coalesced resyncs and open status subscriptions that fail `pane_not_found` for dead panes — but the client must not treat a burst after connect as "live activity", and the review-ready indicator must come only from the snapshot / per-pane path, never from a replayed lifecycle event.)
4. Re-run `session.snapshot` after any reconnect, suspected staleness, or resync trigger (§5.6).

### 5.4 Subscriptions we use

One long-lived **lifecycle connection** with this global, zero-param set:

`workspace.created`, `workspace.updated`, `workspace.closed`, `workspace.focused`, `worktree.created`, `worktree.opened`, `worktree.removed`, `pane.created`, `pane.updated`, `pane.closed`, `pane.moved`, `pane.exited`, `pane.focused`, `tab.focused`, `pane.agent_detected`.

(`pane.focused` / `tab.focused` / `workspace.focused` are consumed as **resync triggers** for the silent done→idle flip, §5.7. `pane.agent_detected` tells us a pane grew an agent worth a status subscription.)

Plus, per agent-bearing pane, one dedicated **status connection** subscribed to `pane.agent_status_changed` for that `pane_id` (required param — **there is no global agent-status stream**; verified, the `pane_id` field is required and non-optional). These are opened when a pane appears (from snapshot, `pane.created`, or `pane.agent_detected`) and closed when it dies (`pane.closed` / `pane.exited`). At the design workload (a handful of agent panes) this is a few extra connections and threads on both sides — cheap.

**[corrected — design change from the predecessor draft]** The draft planned to avoid per-pane subscriptions by diffing `agent_status` on global `pane.updated` events. Source verification kills that: on an agent state change, herdr's `emit_pane_state_update` (`src/app/api.rs:612-661`) emits `pane_updated` **only when the agent's name changed**, and otherwise emits a `PaneAgentStatusChanged` event into the hub — which no *global* subscription type can receive (the `Subscription::PaneAgentStatusChanged` variant requires `pane_id`; verified in the enum definition). So status transitions are structurally invisible to a global-only subscriber. The per-pane subscription is also the only push channel that catches the focus-driven done→idle flip: its poll is hub-first with a `pane.get` snapshot-diff fallback (verified in `ActiveAgentStatusChangedSubscription::poll_result`), and the fallback sees the flipped status even though no hub event was emitted for it. Hence the two-tier connection design above.

Deliberately not used in v1: `pane.output_matched`, `pane.scroll_changed`, `layout.updated`, graphics, plugin methods, `workspace.metadata_updated`, `workspace.renamed/moved/reordered`, `tab.created/closed/renamed/moved`.

### 5.5 Event naming and framing on the wire **[corrected]**

The event stream mixes two envelope shapes; a client must parse `event` as an opaque string and branch on both spellings:

- **Global lifecycle events** arrive as `{"event": "<snake_case>", "data": {"type": "<same snake_case>", ...}}` — e.g. `"pane_created"`, `"workspace_focused"`, `"worktree_removed"`. Underscores, not dots, even though the *subscription request* names them with dots.
- **The three per-pane subscription events** arrive dotted with an **untagged** data object (no `type` field): `{"event": "pane.agent_status_changed", "data": {"pane_id":..., "workspace_id":..., "agent_status":..., ...}}`.
- Pushed event lines carry **no `id` field**; never correlate them to the subscribe request.
- One quirk we exploit: subscribing `pane.agent_status_changed` **with an `agent_status` filter** emits an immediate initial event if the pane already matches. We subscribe unfiltered and seed initial state from the snapshot instead, precisely to avoid double-counting.

Key payloads:

| Wire `event` | `data` fields (required unless marked ?) |
|---|---|
| `pane_created` / `pane_updated` | `pane: PaneInfo` (whole struct) |
| `pane_moved` | `previous_pane_id`, `previous_workspace_id`, `previous_tab_id`, `pane: PaneInfo`, `created_workspace?`, `created_tab?`, `closed_workspace_id?`, `closed_tab_id?` |
| `pane_closed` / `pane_exited` / `pane_focused` | `pane_id`, `workspace_id` only (no PaneInfo) |
| `pane_agent_detected` | `pane_id`, `workspace_id`, `agent?`, `final_status?`, `released?` |
| `workspace_created` / `workspace_updated` | `workspace: WorkspaceInfo` |
| `workspace_closed` | `workspace_id`, `workspace?` |
| `workspace_focused` | `workspace_id` |
| `tab_focused` | `tab_id`, `workspace_id` |
| `worktree_created` | `workspace: WorkspaceInfo`, `worktree: WorktreeInfo` |
| `worktree_opened` | `workspace`, `worktree`, `already_open: bool` |
| `worktree_removed` | `workspace_id`, `worktree: WorktreeInfo`, `forced: bool`, `workspace?` |
| `pane.agent_status_changed` | `pane_id`, `workspace_id`, `agent_status`; `agent?`, `title?`, `display_agent?`, `state_labels?` |

### 5.6 Delivery semantics and the resync rule **[new section — source-verified]**

The event stream has three verified weaknesses, and invariant 9 exists because of them:

1. **Throughput cap:** the server's stream loop delivers at most one event per subscription per 100 ms tick. A burst of 20 lifecycle events drains over ~2 s.
2. **Silent loss:** the server-side event buffer is a 512-entry ring; a subscriber that falls behind loses events with no gap indicator (sequence numbers are not exposed on the wire).
3. **Slow-consumer kill:** if our socket buffer stays full for 5 s, the server tears the connection down.

Contract: the herdr client treats every event as a *hint*, maintains a reconnect loop with full re-bootstrap (§5.3), and performs a **resync** (`session.snapshot`, or targeted `pane.get`/`pane.list`/`workspace.list`) on: reconnect, any `*_focused` event (see §5.7), our own calls to focus methods, and a periodic fallback timer (default 30 s) that heals anything the ring buffer dropped. Resyncs are **coalesced** — at most one `session.snapshot` per 500 ms (trailing edge), and a focus event on a single pane prefers `pane.get` on that pane over a full snapshot — because every snapshot makes herdr inspect every pane's process tree for `cwd`/`foreground_cwd`, and rapid pane-cycling emits several focus events. Status events are **deduplicated against last-known state** (a per-pane subscription can legitimately emit a setup-window event the construction probe already reflected). The review engine itself never depends on the socket at all.

### 5.7 Agent status model **[expanded — source-verified]**

`AgentStatus` enum on the wire: `idle | working | blocked | done | unknown`.

- `done` is derived server-side as exactly `(state Idle, seen flag false)`. It cannot be reported by an agent — the reporting enum has no `done`; only herdr's seen-tracking produces it.
- A completion in the tab the user is currently viewing (with the outer terminal focused) never becomes `done` — it goes straight to `idle` as "seen." `done` therefore means precisely "finished somewhere you weren't looking," which is exactly the review trigger we want.
- `seen` flips true (clearing `done` → `idle`) when the user focuses the tab/workspace, when the outer terminal regains focus with that tab active, or when an API client calls `tab.focus` / `workspace.focus` / `pane.focus` / `agent.focus`. Granularity is **the whole tab**, not one pane.
- **The done→idle flip emits no global event** (verified by tracing all emit sites). A global subscriber will show a stale `done` forever. Our two mitigations: the per-pane `pane.agent_status_changed` subscription (which internally polls and diffs, so it does catch the flip), and the resync-on-`*_focused` rule in §5.6.
- Consequence for our UX: **we cannot clear herdr's `done` flag without changing the user's focus.** There is no "mark seen" API. Ruled at G0 (§10, Q3): lastcall keeps its own local ack state for the review-ready indicator; clicking the dot clears it locally only; a separate explicit "jump to agent" action calls `agent.focus`, which both moves the user there and clears herdr's flag as a side effect.
- `WorkspaceInfo.agent_status` / `TabInfo.agent_status` are max-attention rollups with priority `blocked > done > working > idle > unknown` — usable directly for repo-row status dots when multiple agents share a workspace.
- `working` predicts churn; `blocked` is surfaced as an attention hint. Both render, neither gates anything.

### 5.8 Payload shapes we depend on

**`PaneInfo`** (carried whole by `pane_created` / `pane_updated` / `pane_moved`): required `pane_id`, `terminal_id`, `workspace_id`, `tab_id`, `focused`, `agent_status`, `revision`; optional (omitted when absent) `label`, `cwd`, `foreground_cwd`, `agent`, `display_agent`, `title`, `terminal_title`, `terminal_title_stripped`, `agent_session`, `scroll`; maps `state_labels`, `tokens` (omitted when empty). We consume `agent_status`, `agent`/`display_agent`, `workspace_id`, and `cwd`/`foreground_cwd` (walked up to a known repo root once, for agent-to-repo association only; never used for grouping).

Verified caveats: **`revision` is a presentation-token/title revision only** — it does not move on output, status, cwd, focus, or resize; never use it as a change detector. **cwd fields have no change events** — they are computed fresh on every `pane.get`/`pane.list` but nothing announces a cwd change; our periodic resync covers re-association. `foreground_cwd` is the foreground process-group leader's cwd (unix only).

**`WorkspaceInfo`**: required `workspace_id`, `number`, `label`, `focused`, `pane_count`, `tab_count`, `active_tab_id`, `agent_status`; optional `tokens`, `worktree`. Caveat: `number` is a 1-based positional index that renumbers on reorder — key everything on `workspace_id`.

**`WorkspaceWorktreeInfo`** (the `worktree` field; provenance): exactly `repo_key`, `repo_name`, `repo_root`, `checkout_path`, `is_linked_worktree` — all required when present. **[corrected]** There is no `branch` here; branch lives only on `WorktreeInfo`. Provenance drives worktree badging and parent-repo association.

**`WorktreeInfo`** (in `worktree.list` results and all worktree events): required `path`, `is_bare`, `is_detached`, `is_prunable`, `is_linked_worktree`, `label`; optional `branch` (absent when detached), `open_workspace_id`. Caveat: `label` is the **repo** name, not a per-worktree name; and event-path constructions hardcode `is_bare`/`is_prunable` false — treat those two fields as reliable only from `worktree.list`.

**Worktree events** accelerate our watch set (payloads in §5.5). Emission sequences: `worktree.create` → `workspace_created`, `tab_created`, `pane_created`, `worktree_created`; `worktree.remove` → `worktree_removed` (+ `workspace_closed` if its workspace was open). **[corrected]** These events fire only for worktree operations performed through herdr (verified: every emit site is inside herdr's own create/open/remove handlers). Worktrees created externally — including by an agent running `git worktree add` — are invisible to the event stream, so the periodic rescan (§6.5) runs in integrated mode too, not just standalone. (`worktree.list` does shell out to git on demand and sees external worktrees; it is a valid on-demand probe, just not a push source.) Branch display comes from our own git inspection, not from herdr.

### 5.9 Methods we call

- **`ping`** at startup: liveness + protocol check (client-side enforcement, §4.6).
- **`session.snapshot`** at bootstrap and on every resync.
- **`worktree.list`** (params: `workspace_id?` or `cwd?`) when provenance alone is not enough — enumerates a repo's worktrees with `branch` and `open_workspace_id`.
- **`notification.show`** for "repo ready for review" toasts. Params `{title!, body?, position?, sound?}`; title sanitized+truncated to 80 chars, body to 240 (control chars stripped, whitespace collapsed). Response `{shown: bool, reason}` with reasons `shown | disabled | rate_limited | no_foreground_client | busy`. **Rate limit is one notification per second, globally across all API clients** (single last-shown timestamp, verified `src/app/api.rs:20`), so we coalesce: at most one toast per review-ready burst, and we treat `rate_limited`/`busy` as "drop it," never retry-loop. **Toasts are strictly best-effort and off by default on herdr's side** (review finding F8, verified): herdr's `ToastConfig` defaults to `delivery = "off"`, which returns `disabled` for every call; and under `delivery = "herdr"`, herdr shows its *own* completion toast at the exact moment an agent goes `done`, so our call at that instant returns `busy`. Hence: the in-pane review-ready indicator is the primary signal; the toast is a bonus; docs ship the required `toast.delivery` setting; and we send our toast after a short delay (≥ herdr's toast lifetime) rather than in the same tick.
- Later phases only (stamped now so the client is designed with them in mind):
  - **`pane.report_metadata` / `workspace.report_metadata`** — display tokens (e.g. `$pending` count) in herdr's own sidebar. Verified rules: tokens are per-source **patches** (value sets, JSON `null` deletes, omitted keys untouched, a value normalizing to empty deletes); ≤16 keys/request, ≤32 keys/resource, key `[A-Za-z0-9_-]{1,32}`, values truncated to 80 chars; `ttl_ms` 1..86,400,000 applies to the keys of that patch; `seq` is per-`source`, strictly-increasing, and **stale reports return success-shaped `ok` while being ignored** — so we use one stable `source` (`"lastcall"`) and a monotonic counter, and never rely on detecting a dropped report. Rendering requires the user to add `"$<token>"` to their herdr sidebar config — ship the exact snippet in docs. Tokens are not restored across herdr restarts; re-report after reconnect.
  - **`agent.prompt`** for the flag-and-discuss loop. Params `{target!, text!, wait?: {until: AgentStatus[], timeout_ms?}}`. With `wait` it is atomic: prompt, verify the pane occupant is unchanged, gate on evidence the prompt landed (≤5 s), then wait for `until` (empty defaults to `[idle, done, blocked]`). Error codes to handle by name: `agent_blocked` (agent needs interactive input; nothing is sent), `agent_not_ready` (managed launch pending, or the agent is no longer the pane's foreground process), `empty_agent_prompt`, `agent_not_found`/`agent_target_ambiguous`, `agent_not_running` (pane recycled mid-wait), `timeout`. herdr types the text then submits Enter 300 ms later.

### 5.10 Test-isolation contract (imitated from herdr's own suite)

Every test that talks to a real herdr server uses the pattern from `tests/api_ping.rs`:

- Spawn `herdr server` inside a `portable-pty` PTY, with a per-test temp dir providing `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, and an explicit `HERDR_SOCKET_PATH`; `HERDR_ENV` removed from the child env (avoids the nested-herdr guard); `SHELL=/bin/sh`.
- Wait for the socket by polling `exists && connect` at 25 ms up to 5 s — never sleep-and-hope.
- Client side: a ~50-line newline-JSON reader over `UnixStream` (herdr's `JsonLineReader`, Apache-2.0, adapt with attribution) plus `send_request` (one-shot) and `open_subscription` (persistent) helpers.
- Event assertions collect events **as a set with a deadline**, not in strict order (the 100 ms drain reorders bursts across subscriptions).
- Process hygiene: registry of spawned server PIDs, kill-on-panic hooks, and a matcher that refuses to kill binaries outside the expected target dir.
- The client state machine is additionally unit-tested against a **mock socket server** replaying recorded event streams — deterministic, no herdr binary needed; this is the tier that runs everywhere including macOS CI.

### 5.11 Plugin packaging path (post-v1, verified feasible)

herdr plugins are directories with a `herdr-plugin.toml` manifest (`id`, `name`, `version`, required `min_herdr_version`, `[[panes]]`, `[[startup]]`, ...). A `[[panes]]` entry with placement `split`/`tab`/`zoomed` yields an **ordinary herdr pane** (gets `HERDR_PANE_ID`, participates in all pane APIs) running our binary; `HERDR_PLUGIN_STATE_DIR`/`_CONFIG_DIR` give us blessed storage paths; distribution is `herdr plugin install owner/repo` plus automatic marketplace listing for public GitHub repos tagged `herdr-plugin`. Nothing in v1 blocks this; the binary already self-locates via env vars. Deferred to post-v1 (§8, designed-for list).

---

## 6. Phase 0 contract, part B: our own contracts (v1.0 — frozen 2026-09-01)

### 6.1 Storage layout

State lives under `~/.local/state/lastcall/` (XDG state semantics, used deliberately on macOS too: inspectable with plain `ls`/`cat`/`jq`/`git`, hidden from normal browsing, and not mixed into `~/Library` where terminal users never look). Overridable via `LASTCALL_STATE_DIR`. Config lives at `~/.config/lastcall/config.toml`, overridable via `LASTCALL_CONFIG`.

```
~/.local/state/lastcall/
  roots/<root-hash>/            # one dir per watched parent directory (hash of canonical absolute path; path recorded inside)
    meta.json                   # schema_version, canonical parent path, created_at
    repos/<repo-hash>/          # one dir per repo root or draft root (hash of canonical root path)
      ledger.json               # the seen-state ledger (6.2), written by atomic rename
      store/                    # PRIVATE bare git repository we own: blobs + seen trees (git init --bare)
      store/objects/info/alternates   # -> the user's repo object dir (git roots only)
      index                     # private git index seeded from the seen tree; the fast change detector (6.5)
```

**The private object store is a bare git repository we own.** Baseline blobs and seen trees are written with `GIT_DIR=store git hash-object -w` / `write-tree`, read with `cat-file` / `ls-tree`, and never touch the user's repo. For git roots, `store/objects/info/alternates` (the alternates file lives inside the bare repo's own object directory — harness finding) points at the source repo's object directory (`git rev-parse --path-format=absolute --git-path objects`), so content that already exists in the user's repo — every unchanged tracked file — is referenced, not duplicated; only content absent from the source repo (uncommitted edits, untracked and draft files) is physically written. A read miss (the source repo garbage-collected an unreachable loose object we referenced) fails open per invariant 2. At init, the normalization-relevant config keys (`core.autocrlf`, `core.eol`, `core.filemode`, `core.ignorecase`) are copied from the user's repo into the store's config so both repositories interpret content identically. Draft roots get the same store without alternates. Why git objects instead of a custom blob store: content addressing, packing, and trees for free, plus every piece of our state is inspectable with ordinary git commands (`GIT_DIR=.../store git ls-tree <seen_tree>`).

`config.toml` (v1 keys, all optional, unknown keys are a load error): `parent_dirs` (list of absolute paths; default is the launch cwd. **Launch outside every configured parent dir** (G0 Q4) watches the launch cwd ad hoc for that run, with a one-line notice naming the config file — never an error), `draft_dirs` (globs relative to parent dirs, e.g. `"_drafts/**"`, plus absolute paths), `draft_initial` (`seen | pending`, default `seen` — what a draft root's first sight means, §6.2), `collapsed_globs` (generated files, default includes common lockfiles), `collapse_size_bytes` (default 512 KiB), `ignore_globs` (watch-set noise filters, default includes `.git/**`, `node_modules/**`, `target/**`, `vendor/**`, `.venv/**`), and a `[herdr]` table with `mode` (`auto | on | off`, default `auto`) and `session` (optional named session pin, §6.6). (G0 editorial fix: the draft listed `herdr` as both a scalar and a table, which TOML cannot express.) Two G0 rulings are deliberately **not** config keys in v1: the watcher debounce is hardcoded at 750 ms (Q6) and the accept-all confirmation threshold at more than 10 files (Q5); both are candidates for keys after the Phase 4 circle-back.

**Identity and durability rules (review finding F14):**
- Both `<root-hash>` and `<repo-hash>` are hashes of the **canonicalized** absolute path (`fs::canonicalize`: symlinks resolved, `/tmp`→`/private/tmp`, case as the filesystem reports it). `meta.json` and `ledger.json` record the canonical path; on open, a recorded path that no longer canonicalizes to the same string is treated as a different root (never silently adopted), with a one-line notice.
- Write ordering: objects are written first (git's own temp-file + rename, fsync'd), then the ledger is rewritten by atomic rename. A crash between the two leaves an orphan object (harmless, reclaimed by our own `git gc`) never a dangling ledger reference.
- Verify on read: `cat-file -e` before trusting any referenced object; a missing object fails open to the next resolution step (override → seen tree → empty).

### 6.2 Ledger schema (v1.0) — the seen-tree model

`ledger.json`, one per repo/draft root:

```json
{
  "schema_version": "1.0",
  "root": "/canonical/abs/path/to/repo",
  "kind": "git | draft",
  "seen_tree": "<git tree sha | null>",
  "seen_at": { "head_commit": "<sha|null>", "branch": "<name|null>", "at": "<iso8601>" },
  "overrides": {
    "<root-relative-path>": {
      "blob": "<git blob oid in the private store | null>",
      "mode": "100644 | 100755 | 120000",
      "flag": { "note": "<string>", "created_at": "<iso8601>" } | null,
      "updated_at": "<iso8601>"
    }
  }
}
```

(`blob` is a git object id in our private store; the `sha256` wording from earlier drafts is gone — git's own hashing is the content address.)

**Baseline resolution** for a path `p`, in order: (1) `overrides[p].blob` if the override has a `blob` field (`null` = "seen as absent", the user accepted a deletion); (2) the blob for `p` in `seen_tree`; (3) empty. `pending(p) = diff(baseline(p), worktree(p))`, always computed, never stored. Any failure in resolution (missing object, unparsable override) falls to the next step — the tool over-shows, never hides (invariant 2).

**HEAD is never consulted here.** This is the whole point of the model (review findings F1/F2/F4): the previous draft anchored un-entried files to "blob at current HEAD," which meant an agent's commit silently re-baselined its own edits and every rebase or branch hop invalidated accepts. A content snapshot has none of those failure modes: an agent's commit changes no worktree bytes, so nothing leaves the pile; a branch switch over-shows the branch delta and clears itself on switching back; a rebase that preserves content re-flags nothing.

**First sight of a root** (no ledger yet) sets the initial seen tree explicitly, once, and records it in `seen_at`:
- git root: `seen_tree = HEAD^{tree}` — "you have seen what was committed when you pointed the tool here"; any uncommitted agent work is pending immediately, which is what a post-hoc reviewer wants. A repo with no commits gets `null` (everything pending).
- draft root: per `draft_initial` — default `seen` (a write-tree of the current content; nothing pending until the next change, so a 500-file notes directory doesn't flood on day one), or `pending` (`null`).

**Overrides** exist only where the seen point diverges from the tree: accept-hunk (blob = baseline with the hunk applied), accept-file where the content differs from the tree (blob = the rendered content), accept-deletion (`blob: null`), and flags (a flag may exist on a path with no `blob` field — it does not change the baseline). An override on a path is removed when its blob equals the tree's blob for that path (clean-up, not a semantic change).

**Accept-all (repo)** folds everything into a new tree: build a temporary index from the per-file content hashes snapshotted at confirm time (`git update-index --index-info`, using the rendered blobs — never a fresh read of disk), `write-tree`, set `seen_tree`, clear all `blob` overrides (flags are retained as flag-only). Because the tree is built from what was *rendered*, accept-all blesses exactly what the user saw; a file that changed after the confirm snapshot shows its new delta as pending on the next scan rather than being skipped or silently accepted. **Compaction** uses the same mechanism automatically when overrides exceed a threshold (default 500): the baseline-composed content (tree ⊕ overrides) becomes the new tree. Compaction is invisible to the user and changes no baseline.

**Nothing else is stored.** Annotations (§6.4), pending sets, hunks, counts, and attribution (§6.8) are all derived on scan.

### 6.3 Operations

**Every accept is compare-and-swap too, not just restore.** Each rendered diff carries the content hash (and mode) it was computed from; accept-hunk, accept-file, and accept-all all target *that* hash. If the live file has moved since render, the accept is refused and the view re-renders — otherwise a click could bless content the user never saw, which is the silent-hide invariant 2 forbids (review finding F3).

| Operation | Effect | Writes files? |
|---|---|---|
| accept hunk | Apply the hunk to a copy of the baseline, write the resulting blob to the private store, set the path's override to it. CAS: refused if the file's live hash ≠ the rendered hash. | No |
| accept file | Set the override blob to the rendered content's blob (the hash the diff was computed from — never a fresh read of the live file; if that equals the seen-tree blob, drop the override instead). Records mode. | No |
| accept upstream group | Accept file, iterated over the paths carrying the `upstream` annotation (§6.4), from the group's confirm-time snapshot. | No |
| accept all (repo / global) | Build the new seen tree from the per-file content hashes snapshotted at confirm time (§6.2) and clear blob overrides. Blesses exactly what was rendered; later changes show as new pending. Collapsed-class and upstream-group files are included. | No |
| accept deletion | Set override `blob: null`. CAS: refused if the path reappeared. | No |
| restore hunk / file | Reverse-apply against the working file under compare-and-swap (invariant 4). Write strategy: build the result in a temp file in the same directory (copying mode; symlinks handled per §6.4), re-hash the live file immediately before `rename`, rename atomically. The microsecond hash-then-rename window is a documented residual (deferral ledger); a rescan runs immediately after every restore so any delta that landed in that window shows as pending rather than being hidden. Restoring a pending deletion recreates the file from its baseline blob and asserts absence with a case-sensitive directory listing (not `stat`) so a case-only rename on macOS cannot be clobbered. Restore is disabled for paths in a conflicted state (`U*`/`*U`). | Yes (guarded) |
| flag (with optional note) | Mark the path's override flagged; the hunk plus file path plus note (plus attribution when known, §6.8) is exportable as paste-ready context for handing to an agent. Flagging never changes the baseline. | No |
| editor save (phase 8) | CAS against the content the editor was opened with: if the file changed underneath (an agent wrote while the user edited), refuse the save and offer a re-open/merge rather than clobbering. On a clean save, write the buffer and set the override to the saved blob (invariant 8). | Yes (guarded) |

### 6.4 Git awareness: an annotation layer, never a correctness layer

**Principle:** git history annotates and groups what is pending; it never changes a baseline (invariant 5). Every rule in this section can only ever add a label or a notice — if it is wrong, the user sees a mislabeled pending file, never a hidden one.

- **HEAD tracking:** the git dir is watched (§6.5) and every event schedules a head inspection — `git rev-parse HEAD` and `git symbolic-ref -q --short HEAD` are the truth; the last `logs/HEAD` reflog line (`checkout: moving from main to feat-x`, `commit:`, `rebase (finish)`, `pull: Fast-forward`, `reset: moving to`) is a *hint* used only for the notice text. The displayed branch label is live.
- **Upstream annotation (the flood shaper) — harness-verified classifier.** *Heads:* `HEAD`, plus `MERGE_HEAD` while a merge is in progress (so a half-merged worktree is classified before the merge commit exists). *Range per head:* `seen_at.head_commit..<head>`, falling back to `merge-base(seen_at.head_commit, <head>)..<head>`; no merge-base → no annotation. *Per commit in range:* it is **upstream** iff it is reachable from a remote-tracking ref (`git rev-list -n1 <c> --not --remotes` prints nothing) **and** neither its author nor its committer email is the user's `user.email` — the identity test is what keeps an agent's own pushed commits (B9) from being grouped away as "reviewed elsewhere"; it is otherwise **local**. Paths touched by upstream commits form `U`; paths touched by local non-merge commits form `L`; paths listed by `git diff-tree --cc` of a *local merge commit* form `M` — `--cc` lists paths that differ from **all** parents, i.e. files both sides changed, which covers conflict resolutions and clean auto-merges of the same file. *Per pending path:* in `U` but not `L`/`M`, and worktree content equals that path's blob at some head → tag **`upstream`**; in `U` and (in `L` or `M`, or content differs from every head) → badge **`includes upstream changes`** ("mixed"); otherwise plain. Tagged paths render as a single collapsed **"upstream · N files"** group row with one accept (§6.7); mixed paths are individual rows with the badge, and at hunk granularity a hunk whose new side matches a head's content for that region carries the same label (best-effort, UI phase). Any doubt (detached HEAD with no upstream, shallow clone, no remotes) → untagged individual rows. Verified outcomes: a clean `merge origin/main` with uncommitted work shows the uncommitted files individually and upstream grouped with the merge contributing nothing (C3); conflict resolutions show individually with the badge both during and after the merge (C4); a file both sides edited in different regions shows as mixed (C5); an upstream file with an agent edit on top shows as mixed (C6); a rebase onto upstream re-flags nothing but the upstream group (B5).
- **Why this is safe where the anchor model was not:** because the baseline is content, a push changes nothing (the sticky-classification problem does not exist), a pull's 40 files are still pending until accepted (just grouped), and a mistaken tag costs one click of un-grouping, not a hidden change.
- **Branch switch:** the delta between branches over-shows with the notice "switched main → feat-x: N files differ from seen state"; switching back clears naturally because content matches the seen tree again. A per-branch map of seen trees (so a never-reviewed branch you hop to doesn't flood while you're there) is a designed-for refinement, recorded in the deferral ledger with a trigger.
- **History rewrite (rebase, amend, reset):** nothing dies. Overrides are content-keyed and stay meaningful; content the rewrite changed shows as pending; content it preserved does not. The notice reads from the reflog ("rebased feat-x onto main").
- **Path enumeration:** from the private index (§6.5): `git diff-files` against the seen-tree-seeded index gives every path whose content or mode differs from the seen tree; `git ls-files --others --exclude-standard` (with the draft-dir globs force-included) gives new files; override paths are always included. Never from walking HEAD's tree against disk. Sparse-checkout is handled explicitly: skip-worktree bits live in the **user's** index, not ours, so candidates are filtered against `git -C <root> ls-files -v` entries tagged `S` (harness finding: without this filter every sparse-excluded file showed as a false pending deletion).
- **Content model:** for git roots, worktree content is hashed and diffed **as git sees it** — through the clean filter, so our blobs are canonical git blobs and our line counts match `git diff`; raw-byte diffing would show whole-file CRLF churn. **Exact invocation matters (harness-verified):** run `hash-object -w` with the process cwd at the root and a root-relative path (`GIT_DIR=store GIT_WORK_TREE=<root> git -C <root> hash-object -w -- <relpath>`). The `--stdin --path=<p>` form and absolute paths do **not** apply `text=auto` normalization when invoked from outside the work tree — the harness produced a CRLF blob (3/3 numstat churn) that way and the correct LF blob (1/1) with the cwd-relative form. Symlinks are hashed from their link text via `--stdin` (no normalization applies). Draft roots outside any git repo use raw bytes. Overrides record the file **mode**; a mode-only change (`chmod +x`) is a pending change with a one-line "mode 100644 → 100755" hunk, accepted like any other, honoring `core.fileMode`.
- **Symlinks:** always `lstat`. A symlink is modeled as git does — mode `120000` with the link text as content — never by reading through it. Restore of a symlink is `unlink` + `symlink`; no file is ever opened for writing without `O_NOFOLLOW`.
- **In-progress operations:** while `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `rebase-merge/` or `rebase-apply/` exist in the git dir, the engine keeps computing pending (conflict markers show as content), suppresses transition notices until the operation completes, and disables restore for conflicted paths.
- **Deletions and renames:** deletions per §6.2/§6.3. Renames render as delete-plus-add, visually paired: `git diff --find-renames` only pairs tracked↔tracked, and an agent's `mv old new` is `D old` + `?? new`, so we pair by similarity ourselves (git's own algorithm via a temporary index: `git add -N` then `git diff -M`). Carrying an override across a rename is a later refinement; the path-keyed schema permits it without a schema break.
- **Nested git:** the nearest enclosing `.git` (directory or worktree pointer file) owns a path. Submodules and vendored repos resolve to themselves, not the outer repo.
- **Worktrees:** each linked worktree is its own root with its own ledger and private store (alternates point at the shared common object dir). Parent association (badge, org/repo line) comes from the worktree's `.git` pointer and, when available, herdr provenance. A linked worktree's `HEAD`/`index` live under the common dir's `worktrees/<name>/`, not under the root — watching follows the pointer (§6.5).

### 6.5 Watcher and refresh

`notify` (FSEvents) watches all repo roots and draft dirs, filtered by `ignore_globs`. **`ignore_globs` scopes the watcher only** — pending is always computed from `git status`, so an agent edit to a tracked file under `vendor/` is never hidden, merely discovered on the next scan rather than the next event. Events are debounced per-path (750 ms of quiet, hardcoded in v1 per G0 Q6) and only ever trigger recomputation, never state changes (invariants 1 and 9). Full rescan happens at startup, on watcher overflow/error, and on demand (manual refresh key).

**HEAD movement is watched explicitly.** Commits, checkouts, rebases and pulls touch only the git dir, which the default `ignore_globs` excludes — so without this, the git state machine (§6.4) would never run until some unrelated file changed (review finding F5). For each root, resolve `git rev-parse --git-dir` and `--git-common-dir` (they differ for linked worktrees) and watch, non-recursively, an allowlist: `HEAD`, `index`, `ORIG_HEAD`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `packed-refs`, `refs/` (recursive), `rebase-merge/`, `rebase-apply/`. Any event there schedules a HEAD/branch re-inspection for that root. A periodic per-repo HEAD poll (default 10 s) is the invariant-9 backstop for missed events. The per-repo scan uses the **private index** (§6.1): seeded from the seen tree (`git read-tree <seen_tree>` under `GIT_INDEX_FILE`), refreshed each scan with `git update-index -q --refresh` so git's stat cache makes the common case a stat-only pass, then `git diff-files -z` (changed/deleted vs the seen tree) plus `git ls-files --others --exclude-standard -z` (new files; draft-dir globs force-included) — plus `git status --porcelain=v2 -z` for conflict/in-progress state and HEAD/branch inspection for annotations. Draft roots outside any git repo run the same plumbing with our private store as the repository and the draft dir as `GIT_WORK_TREE`, so there is one scan implementation, not two. Repo-root discovery: startup scan of `parent_dirs` for `.git` entries, plus a **periodic light rescan of `parent_dirs` for new/removed roots in all modes** — herdr worktree events accelerate discovery when present but cannot replace the rescan, because they fire only for herdr-initiated worktree operations (§5.8); agents create worktrees directly with git.

### 6.6 herdr client behavior

A dedicated task owns all socket connections and translates herdr state into engine messages; nothing in the review engine depends on it.

- **Connection topology:** one lifecycle-subscription connection (§5.4 set), one `pane.agent_status_changed` connection per agent-bearing pane (opened/closed with the pane), plus one-shot connections for `ping`/`session.snapshot`/`worktree.list`/`notification.show` calls. Reconnect-with-rebootstrap loop per §5.6; a resync (snapshot + reinstall) runs on reconnect, on `*_focused` events, and on the 30 s fallback timer.
- **Per-pane status connection lifecycle (review finding F9):** a subscribe that fails at construction (`pane_not_found` — the pane died between the lifecycle event and our subscribe) means "pane gone, do not retry"; only transport-level failures trigger the reconnect loop. Once open, herdr never errors a status stream for a dead pane — its poll swallows `pane.get` failures and the connection stays up silently — so every resync reconciles our open status connections against `session.snapshot.panes` and tears down any whose pane is absent. This is the only defense if the `pane_closed` event itself was dropped by the ring buffer.
- **Session discovery:** in a pane, `HERDR_SOCKET_PATH` is authoritative. Standalone, try the default socket, then enumerate `<config_dir>/sessions/*/herdr.sock` and connect to the live one (`ping` succeeds); a `herdr.session = "<name>"` config key pins one explicitly. Multiple live sessions with no pin → connect to none, show a "multiple herdr sessions, set herdr.session" notice.
- **Agent-to-repo association:** walk the pane's `foreground_cwd` (fallback `cwd`) up to a known root; unresolvable panes are ignored. Association refreshes on resync (cwd changes emit no events, §5.8). If exactly one agent maps to a repo, the repo row shows that agent's status; multiple agents degrade to the workspace-level max-attention rollup or a neutral multi-agent indicator (stamped degradation rule).
- **Review-ready flag:** herdr `done` sets the repo row's review-ready indicator and (rate-limit-aware, coalesced) fires one `notification.show` toast. Clearing is **local ack** (§5.7): clicking clears our indicator only; the explicit "jump to agent" action calls `agent.focus`. If herdr still reports `done` after a local ack, the row shows the acked-but-unvisited state quietly (dimmed dot), not a re-alert.
- **Disconnects** flip the UI to standalone with a visible badge and a reconnect loop.

### 6.7 UI contract (lean by design)

Layout: header strip, left nav, main view, one-line status bar.

- **Header:** watched-repo count, total pending files/hunks, Accept All (confirmation dialog only when the action covers more than 10 files, per G0 Q5; the confirm shows the file count and, when any are grouped or collapsed, says so), herdr connection badge.
- **Left nav** (default ~28 cols, mouse-draggable width, scrollable): repos stacked with a horizontal-rule delimiter, appearing only when they have pending files or an attention flag. Repo row: bold directory name; optional dimmed `org/repo` (from `git remote get-url origin`, ellipsized, toggled by a shortcut, hidden when no remote); status dot mirroring herdr state (`done` renders as the review-ready flag, cleared per the local-ack rule); worktree badge with parent name where applicable. Beneath: branch line (live label, from our git inspection). Beneath: file rows, name-only or full relative path (shortcut toggle), each with green added / red deleted counts updating live, collapsed-class files marked and rendered as a single accept row, flagged files marked.
- **Main view:** click a file to open its diff, live-updating as the file changes (re-render on debounce; scroll position preserved best-effort). Hunk navigation next/previous via keys and click; per-hunk accept, restore (guarded), and flag controls; whole-file accept. Phase 8 adds inline editing; until then an "open in $EDITOR at this line" action covers the fix-a-comment case.
- **Interaction:** every action is reachable by both mouse (click, wheel, drag) and keyboard; keybindings are config-stubbed but ship with defaults. Accepting the last hunk of a file advances to the next file; accepting a repo's last file collapses the repo out of the nav. That "the pile visibly shrinks" feel is the core UX loop and is a requirement, not a nicety.

---

### 6.8 Attribution layer (designed-for; post-v1)

Change detection is the filesystem and git — that is what makes the tool agent-agnostic (invariant 7). Agent hooks are an **additive metadata source** layered on top, in exactly the way the herdr socket is additive: when present they enrich, when absent nothing degrades.

- **What a hook adapter provides:** for a file edit made through an agent's structured tools (Claude Code `PostToolUse` on `Edit`/`Write`/`MultiEdit` first; others as their hook APIs are verified), an attribution record `{path, agent, session_id, at, tool}` and, where the payload includes it, the exact before/after content. Attribution is stored as derived metadata keyed by path in a sidecar (`attribution.json`, TTL'd), never in the ledger, and never affects baseline resolution or pending computation.
- **What it enables:** the pending row shows "last touched by Claude Code · session abc" ; the flag export names the agent; the post-v1 flag-and-discuss loop can route a flagged hunk to the specific session (via herdr `agent.prompt` when the session maps to a pane, or by paste otherwise).
- **What it explicitly does not do:** it is not a change-detection path. Bash-mediated edits (formatters, generators, `sed -i`, package managers) have no per-file hook payload and are caught by the watcher like everything else; a hook that fires for a file the watcher hasn't seen yet just schedules a rescan.
- **Adapter contract:** each adapter is a tiny shell/JSON shim registered in the agent's own hook config, posting to a local endpoint lastcall exposes (Unix socket, newline JSON, same shape as our herdr client's reader). Adapters live in `integrations/<agent>/` and are documented per agent with the exact config snippet. Adding an agent never touches the engine.
- **Scheduling:** post-v1, planned on the facts below (researched 2026-09-01 against each agent's current docs and herdr's integration sources).
- **Agent hook landscape (verified against documentation, 2026-09-01):** file-level post-edit hooks with path + old/new content + session id + cwd, shell-invoked with JSON on stdin, exist **with zero guessing** in Claude Code (`PostToolUse` matcher `Edit|Write|MultiEdit`; `~/.claude/settings.json`), Gemini CLI (`AfterTool` on `replace`/`write_file`), Factory droid (`PostToolUse` `Edit|Create`), and Qwen Code (`PostToolUse` `edit|write_file`). The same data is available **in-process only** (a JS/TS or TS module we'd ship, not a shell hook) in opencode and Kilo (`tool.execute.after` with `metadata.filediff.patch`, bus event `file.edited`) and pi (`tool_result` for `edit`/`write` with `details.patch`). Cursor CLI has `afterFileEdit` with `file_path` + `edits[{old_string,new_string}]` and a `conversation_id` but no cwd, and its CLI hook support rests on changelog entries with an open Linux bug. Codex CLI fires `PostToolUse` for `apply_patch` but delivers the **raw patch text** in `tool_input.command` (parse `*** Update File:` hunks ourselves). Copilot CLI's `postToolUse` has session id and cwd but an undocumented `toolArgs` shape for edits. Kimi, Qoder, Mistral Vibe and Hermes have generic post-tool hooks with partially documented file-tool payloads. Bash-command capture (`tool_input.command`) is documented for every shell-hook agent. Two traps: write-style tools deliver **new content only** (old content must come from our own seen state, which we have anyway); and Claude-format `settings.json` hooks are also read by Cursor CLI and Copilot CLI, so one hook file can fire from three agents — attribution keys on `hook_event_name` plus the presence of `cursor_version`/`CURSOR_VERSION`, exactly as herdr's own integration does, never on the file the hook was installed from.
- **Adapter order:** Claude Code → Gemini CLI → droid/Qwen (same shape) → opencode/Kilo/pi (in-process module) → Cursor → Codex (patch parser) → the rest as their payloads are documented. Each adapter ships with a recorded fixture of its real payload and a unit test; the fixture is the contract.

## 7. Testing strategy

### 7.1 Unit tier (`just test-unit` — no network, no clones, deterministic)

The engine is a library crate driven by tests. Core coverage: baseline resolution (default rule, scope validity, ancestry rule), hunk apply-to-baseline math, CAS restore guard, deletion/rename representations, ledger serialization round-trips, fail-open behavior on corrupt blobs/entries, and the herdr client state machine driven by a **mock socket server** replaying recorded event streams (both envelope shapes, out-of-order bursts, mid-stream disconnects, ring-buffer-loss scenarios healed by resync). Property tests (proptest) on the invariant "accept-hunk then recompute never resurrects an accepted hunk and never drops an unaccepted one" across randomized edit sequences. This tier is the ratcheting floor (§4.4) and runs identically on macOS and Linux.

### 7.2 Integration tier (`just test-integration` — real git, real herdr binary)

Scripted scenarios against real git: fixture repos `git init`-ed locally from vendored tarballs, fully offline — no network fixtures in CI (review finding F13: clone-based GitHub fixtures would need repos that don't exist and network+auth on runners for no added coverage). "Remote" scenarios (fast-forward pull, upstream-reachable commits) use a second local bare repo as `origin`. Scenario scripts drive the exact failure cases from this spec: edit-then-branch, branch flip-flop at same commit, commit-then-review, fast-forward pull, rebase kill, delete, rename, lockfile collapse, draft-dir snapshots, mid-review file mutation (CAS refusal), crash-recovery (kill mid-accept, restart, assert recompute), nested-repo ownership, linked-worktree roots.

herdr-facing integration uses the §5.10 isolation recipe against the **pinned** herdr release: bootstrap, event naming (both shapes asserted), per-pane status subscription lifecycle, done-flip resync, worktree add/remove driving the watch set, notification rate-limit handling.

### 7.3 End-to-end tier (`just test-e2e`)

Two layers. First, Ratatui's `TestBackend` with `insta` snapshot assertions renders real frames from real engine state (nav layout, counts, hunk view) without a terminal. Second, PTY-level tests using `portable-pty` drive the built binary in a real pseudo-terminal, send key/mouse input sequences, and assert on screen contents — herdr's own suite is the pattern reference (spawn-in-PTY, poll-for-ready, set-based event assertions, PID hygiene). PTY tier is CI-blocking on Linux, best-effort on macOS runners (§4.5).

### 7.4 Scenario harness (the executable spec)

[`01-scenarios.md`](01-scenarios.md) enumerates every git/draft/herdr scenario this spec makes a claim about, each as a setup script, an action, and the exact expected pile (paths, annotations, group rows, notices) before and after a restart. Before Phase 2 is kicked off, a throwaway shell harness runs the git-plumbing half of each scenario (private store, alternates, private index, `diff-files`, `index-info` + `write-tree`, upstream classification) against real git and asserts the expected sets — the model is verified executable before a worker builds on it. The harness then becomes Phase 2's integration fixtures verbatim. **Status 2026-09-01: built and green** — `scripts/harness/{lc.sh,scenarios.sh}` (run `bash scripts/harness/scenarios.sh`), 46 assertions across A1–A7, B1–B9, C1–C6, D1/D3/D6/D10/D11, E2/E5, F2/F3, all passing against git 2.37 on macOS. Building it surfaced four precise rules that are now in §6.1/§6.4 (alternates location, cwd-relative hashing for `text=auto`, skip-worktree from the user's index, `MERGE_HEAD` + `--cc`-as-mixed in the classifier) — each was a spec bug caught before a worker existed.

### 7.5 herdr compatibility

The scheduled Action (Phase 5, moved from Phase 9 on 2026-09-01) installs the latest herdr release, runs the real-server integration subset, diffs `herdr api schema --json` against our committed consumed-surface fixture, and opens an issue on drift. Until it exists, the Phase 5 circle-back (§3.5) covers this manually.

---

## 8. Roadmap: phases and gates (written in advance)

Each phase is one PR (dozens of commits acceptable), independently testable, delivering visible value. Gate items state their **evidence form**; "it works" is never evidence. Standing items for every phase from 2 on: full suite green at or above the recorded floor (number stated in each kickoff spec; Phase 2's kickoff states "≥ the count at Phase 2 open" and its close records the first hard number), lint clean, no `deny_unknown_fields` on herdr-facing types (grep), no weakened/deleted tests (verifier class).

Gate items marked **[sponsor]** require a human at a real terminal/herdr session or a real agent run; an autonomous worker cannot evidence them. They are performed by the sponsor at phase close, the transcript pasted into the PR, and the worker's obligation is to make the demonstration one command away (a scripted demo, a documented recipe). A worker must never self-report a [sponsor] item as done (review finding F13).

### Phase 0 — Spec stamped (G0)
**Deliverable:** this document reviewed, §9 questions ruled, contracts frozen at v1.0, decision log initialized. No product code.
**Gate — closed 2026-09-01:**
- [x] Every §9 question carries a `[DECIDED]` entry in §10 with the verbatim ruling (Q1–Q8; Q7 decided earlier the same day).
- [x] Name (`lastcall`) and license (MIT OR Apache-2.0) recorded; local directory and GitHub repo already renamed to `lastcall`.
- [x] Amendment log initialized (empty). Corpus committed at `docs/spec/` on branch `feat/phase1-scaffold` (commit hash in the PR).

### Phase 1 — Scaffold + hello-herdr
**Deliverable:** workspace crate layout (engine lib + TUI bin + testkit), `rust-toolchain.toml`, `justfile` targets (including `herdr-fetch`, §4.4), CI (lint/unit/integration jobs, macOS+Linux), permission allowlist committed, fixture repos **generated by script** (offline and deterministic — the harness's `mk_repo` proved tarballs unnecessary; G0 editorial change from "tarballs vendored"), license files, `AGENTS.md`, `z_ignore/` in the repo `.gitignore`. **Config layer** (`config.toml` loading with all §6.1 v1 keys, `LASTCALL_STATE_DIR`/`LASTCALL_CONFIG` overrides, defaults, validation errors) — every later phase reads config, so it lands first. **Mock herdr socket server** (test-support crate: serves the request/response envelopes, replays recorded event streams in both envelope shapes, can drop/close/delay) — required by the client state-machine unit tests from this phase on. Plus a hello-herdr binary: connect, ping-with-protocol-check, bootstrap snapshot, stream lifecycle events, open per-pane status subscriptions, print agent status transitions to stdout.
**Gate 1 closed 2026-09-01, PR #1 merged `692e43a`** (kickoff: `docs/spec/90-phase1-kickoff.md`; worker half closed at `d5a6e38`; orchestrator re-ran every item personally; CI green on both OSes, runs 33583304045 and 33583758742):
- [x] `just build && just lint && just test-unit` green locally on macOS: 101 unit tests (engine 85, testkit 16, bin 0; after the orchestrator's review-fold commit); `ci.yml` runs exactly those targets plus `just test-integration-herdr` on `ubuntu-latest` and `macos-latest`. CI-run link: attached to the PR after push.
- [x] Client state machine driven by the mock: 30 `client_*` tests in `crates/lastcall-engine/src/herdr/client.rs` (in-memory transport, paused time) plus 7 transport tests over the socket mock.
- [x] Config round-trip: 19 `config_*` tests in `crates/lastcall-engine/src/config/`, including the nine named in the kickoff; `just probe-config` prints the effective config from the release binary.
- [x] Isolated-herdr integration test `herdr_real_ping_bootstrap_events_and_done_derivation` green on macOS via `just test-integration-herdr` (ping 0.8.2/protocol 20, bootstrap, per-pane `[Working, Done]`, `done → idle` after `tab.focus`, subscription refusals); Linux CI leg confirmed after push.
- [x] **[sponsor]** Hello-herdr in a real herdr session: recipe at `docs/dev/hello-herdr.md`; automated proxy `just probe-hello` shows both transitions. Transcript pasted on PR #1 by the sponsor on 2026-09-01 (`unknown → working`, `working → done`, `done → idle` via the per-pane poll before `tab_focused`).
- [x] Allowlist present; orchestrator read the worker report: no permission prompt fired (two commands were split by the worktree-isolation guard, which is unrelated to the allowlist).

### Phase 2 — Headless engine
**Deliverable:** root discovery (incl. periodic rescan), the private object store with alternates and the private-index scan (§6.1/§6.5), the seen-tree ledger with overrides, first-sight rules, accept-all/compaction via `update-index --index-info` + `write-tree` (§6.2), git-normalized content hashing with mode tracking, symlink model, git-dir HEAD watching, the upstream annotation and transition notices (§6.4), draft roots on the same plumbing, pending computation, ledger persistence with atomic writes, storage identity rules, crash recovery. Exposed as `lastcall status [--json]` (which reports annotations and groups, not just paths). The scenarios document (`lastcall-scenarios.md`) is the test plan: every scenario becomes a named integration test.
**Gate 2 closed 2026-09-02 at `dca32cd` on `feat/phase2-engine`, pending PR #2 merge** (kickoff: `docs/spec/91-phase2-kickoff.md`, design review folded at `52fcffe`; worker delivered 21 commits, two in-worktree verifier passes, then the orchestrator's adversarial code review — 14 findings, 12 folded as `fix(review):` commits `d131f88`/`fd6795a`, 2 recorded below; the orchestrator re-ran the suite, the greps, both probes and the three gating fixes personally):
- [x] Full integration suite for the git state machine passes: 45 named scenario tests in `crates/lastcall-engine/tests/test_integration_scenarios_{a..f}.rs` (`scenario_<id>_<slug>`, A1–A8, B1–B10 incl. `b5b_pull_rebase` and `b10_rebase_stopped_on_conflict`, C1–C8, D1–D11, E1–E5, F1–F3; every (H) expectation copied verbatim from `scripts/harness/scenarios.sh`), plus 3 watcher and 1 golden integration tests. Commit-then-review = `scenario_b1_commit_does_not_clear_pending`; upstream group = `scenario_c1_c2_fetch_then_ff_pull`; agent push = `scenario_b9_push_keeps_individual_rows`; rebases = B5/B5b/B10; branch switch = B2–B4; symlink D2, mode-only D1, CRLF D3, sparse D6, in-progress merge C4, rename D5.
- [x] Property tests `hunks_accept_then_recompute_never_resurrects_or_drops` and `hunks_apply_all_in_any_order_yields_current` green at `ProptestConfig { cases: 1000 }` (default persisted RNG; no `proptest-regressions/` file appeared).
- [x] Crash recovery `scenario_e1_kill9_between_object_write_and_ledger_rename`: re-exec'd child SIGKILLs itself at `AfterObjectWrite` and at `AfterLedgerTmpWrite`; ledger bytes identical, novel orphan blob present, reopened pile equals the pre-kill oracle, accept succeeds after reopen.
- [x] HEAD-movement detection: `watcher_commit_without_file_activity_triggers_head_inspection` (integration tier, `head_poll` 250 ms) and the sponsor-visible `just probe-watch` line `alpha  committed on main (1 commit)` with the committed file still pending (invariant 5).
- [x] `status --json` golden: `status_json_multi_repo_matches_golden` against `crates/lastcall/tests/golden/status_multi_repo.json` from the built binary (three roots: B1 repo, C2/C6 repo, F3 draft dir); byte-stable under `TZ`/`LANG` changes and a different cwd; `just probe-status` prints the same document.
- [x] Unit-test floor recorded: **182** (engine 166, testkit 16, bin 0) — the Phase 3 floor. PR #2 CI (Linux integration tier) then caught a Linux-only scan loop — `notify`'s inotify backend reports `IN_OPEN`, so the scan's own reads re-triggered it; fixed on the branch before merge with one unit test (181 → 182). One deliberate dip inside the phase (162 → 159) moved three FS-live watcher tests to the integration tier where `docs/dev/testing.md` says they belong; the tier now runs in ~5 s.
- [ ] **[sponsor]** CI green on both OSes after push (run links on PR #2), and `just probe-watch` seen once on the sponsor's machine (the `committed on main` notice). Host note: this Mac's `fseventsd` delivered no filesystem events during most of the build (137 days up, ~7 GB RSS); the engine's polling backstops covered it and the one event-delivery test self-skips visibly when that happens.

### Phase 3 — Read-only TUI
**Deliverable:** header, left nav (repos, branch lines, file rows with live counts), diff view with hunk navigation, watcher-driven live updates, mouse plus keyboard. No accept yet.
**Gate:**
- [ ] TestBackend snapshot suite covers nav layout, counts, diff view, empty state (snapshot names listed).
- [ ] Live-update demo: scripted file mutation appears in the running TUI within debounce+1 s (PTY test or recorded transcript).
- [ ] Mouse and keyboard paths both exercised in tests for: select repo, select file, hunk next/prev.

### Phase 4 — Seen-state (the product moment)
**Deliverable:** accept hunk/file/all, entry persistence, restart resume, the shrink-the-pile loop.
**Gate:**
- [ ] **[sponsor]** End-to-end: a real agent session's edits reviewed post-hoc to zero pending, restart shows zero pending (transcript).
- [ ] Scripted equivalent of the above using a fixture "agent" script (edits + commits), including restart across a commit (integration test).
- [ ] Accept CAS: file mutated between render and accept → accept refused, re-render, nothing hidden (test).
- [ ] Accept-last-hunk advances file; accept-last-file collapses repo (TestBackend snapshots).
- [ ] Property suite green including accept-all paths; floor ratcheted.
- [ ] Circle-back scheduled: ack semantics + accept-all confirm reviewed against real use (§3.5).

### Phase 5 — herdr in the UI
**Deliverable:** status dots, review-ready flag with local ack, jump-to-agent (`agent.focus`), worktree events driving the watch set, connection badge and standalone fallback, `notification.show` toasts (coalesced, rate-limit-aware).
**Gate:**
- [ ] **[sponsor]** Agent finishes in left pane → repo flags in right pane (and, with `toast.delivery` configured, a toast): demonstrated against a real herdr session (transcript/recording). Toast is best-effort per §5.9; the in-pane flag is the gating signal.
- [ ] Done-flip staleness healed: with the lifecycle stream artificially deprived of the flip, resync clears the stale flag within the fallback interval (integration test against pinned herdr).
- [ ] Disconnect mid-session → standalone badge → reconnect → state converges with snapshot (integration test).
- [ ] Orphan status connections reconciled: pane closed with its `pane_closed` event suppressed → connection torn down at next resync (mock-server test).
- [ ] Mock-server unit tests cover both envelope shapes, subscription-failure-closes-connection, resync coalescing, and event dedupe (test names).
- [ ] Manual §5 re-verification against the current herdr **release tag** recorded in PR (circle-back §3.5).
- [ ] **Scheduled herdr-compat workflow live** (moved here from Phase 9 by the 2026-09-01 protocol ruling, §10): installs the latest herdr release, runs the real-server integration subset, diffs `herdr api schema --json` against our committed consumed-surface fixture, opens an issue on drift; has run green at least once (run link).

### Phase 6 — Draft dirs + collapsed classes
**Deliverable:** draft-dir ledger (watch, snapshot, review non-git files with the same flow); collapsed classes (binary, size threshold, lockfile globs with single-row accept).
**Gate:**
- [ ] Gitignored draft file goes pending → hunk-reviewed → accepted → survives restart (scenario test).
- [ ] A lockfile churn produces exactly one accept row, not thousands of hunks (snapshot).
- [ ] Binary and >512 KiB files render as collapsed rows (snapshot).

### Phase 7 — Restore + flag
**Deliverable:** restore with CAS (hunk and file, including deletion-restore); flag-with-note plus paste-ready export.
**Gate:**
- [ ] Mid-review mutation test: file changes between render and restore → refusal + re-render, file untouched (byte-compare evidence).
- [ ] Restore of a pending deletion recreates the file identical to baseline blob (hash compare).
- [ ] Flagged hunk exports path+hunk+note (golden-file test); **[sponsor]** round-trip demonstrated by pasting to an agent (transcript).
- [ ] Symlink restore never writes through the link; restore refused on conflicted paths (tests).
- [ ] Verifier run targeting "restore without CAS," "restore clobbers newer content," "restore follows symlink" — findings + resolutions in PR.

### Phase 8 — Editing
**Deliverable:** open-in-`$EDITOR`-at-line first, then inline edit widget; baseline advances on save (invariant 8).
**Gate:**
- [ ] Editor save produces zero new pending for the saved content (test).
- [ ] `$EDITOR` launch lands at the correct line (integration test with a probe editor script).

### Phase 9 — Hardening and release
**Deliverable:** PTY e2e suite complete, release Actions with signed binaries, `docs/` filled, README, deferral-ledger review.
**Gate:**
- [ ] **[sponsor]** Tagged v0.1.0 installable from GitHub Releases on a clean machine (install transcript); worker provides a scripted fresh-container install smoke as the automated proxy.
- [ ] Scheduled compat job (live since Phase 5) still green on the release's herdr pin; any drift issues it opened are closed or ledgered.
- [ ] Deferral ledger reviewed; every open entry re-affirmed or scheduled (decision-log entry).
- [ ] Docs: install, config reference, herdr setup (incl. sidebar-token snippet), review-loop walkthrough.

**Designed-for but explicitly post-v1:** `agent.prompt` flag-and-discuss loop, `report_metadata` pending-count tokens in herdr's sidebar, herdr plugin packaging (§5.11), **hook-based attribution adapters (§6.8; Claude Code first)**, per-branch seen trees, rename override carry, Homebrew tap, Windows.

---

## 9. Open questions for G0 — all ruled 2026-09-01

Kept for the record; the rulings are in §10 under "G0 bulk stamp".

1. ~~**License.**~~ → MIT OR Apache-2.0 dual (§1.5). Directory and GitHub repo were already renamed to `lastcall` before G0.
2. ~~**Spec home.**~~ → committed in full at `docs/spec/` (nothing in the corpus is private).
3. ~~**Ack semantics.**~~ → local ack + explicit jump-to-agent (§5.7, §6.6).
4. ~~**Launch outside a configured parent dir.**~~ → ad-hoc watch of cwd with a notice (§6.1).
5. ~~**Accept-all confirmation.**~~ → confirm only above 10 files (§6.7); hardcoded in v1.
6. ~~**Debounce default.**~~ → 750 ms, hardcoded in v1 (§6.5).
7. ~~**Ledger anchoring model.**~~ → seen-tree model (§10, decided earlier the same day).
8. ~~**Agent-status strategy.**~~ → per-pane subscriptions + resync ratified as the §5.4 contract.

No question is left `[OPEN]`-pending-data. Items 5 and 6 are re-examined at the Phase 4 circle-back (§3.5).

## 10. Decision log

(append-only; entries: date, question, verbatim ruling, tag `[OPEN]`/`[LEANING]`/`[DECIDED]`, supersedes-pointer if any)

- **2026-09-01 — Project name** `[DECIDED]`: **lastcall**. Ruling (sponsor, in session): "lastcall makes sense." Context: "ledgr" is namespace-crowded on GitHub (multiple active finance apps) and semantically claimed by accounting tools; "accptr" is unique but hard to spell (double-c, dropped vowels); themed alternatives (`onceover`, `signoff`, `finalcut`, `shipcheck`, `lgtm`) are all taken by established projects; `lastcall` had only trivial unrelated collisions and matches the product framing — the last call before code ships. Two-way door until the repo goes public in Phase 1.
- **2026-09-01 — Ledger baseline model** `[DECIDED]`: **seen-tree model** — the baseline is a content snapshot (a git tree in a private object store plus per-path overrides); HEAD is never consulted for correctness; git history only annotates/groups (upstream group row, "includes upstream changes" badge, transition notices). Supersedes the original "blob at current HEAD" default rule (review F1/F2/F4: an agent commit hid its own work; ancestry could not distinguish agent commits from upstream pulls; rebase/branch-switch killed accepts) and the interim "reviewed anchor + sticky local-commit set" proposal presented the same day, which the sponsor and orchestrator judged to be trading hook complexity for worse git complexity. Sponsor ruling (verbatim, in session): "yes, I agree with you." One-way-ish door: the on-disk schema is built around it; revisit only with a schema bump.
- **2026-09-01 — Agent hooks** `[DECIDED]`: hooks are an **additive attribution layer**, not the change-detection foundation (§6.8). Sponsor ruling (verbatim): "hooks can be an extra layer of metadata and attribution, and then we can determine at a future point just how much effort it is and the value we get from it ... still move forward with adding hook support at a later point in the phases." Rationale recorded: hooks cannot see Bash-mediated changes (formatters, generators, sed, package managers) without a filesystem diff anyway; agent coverage would require per-agent integrations (herdr's model, not ours); the seen-tree model removed most of the git complexity that motivated the question. Two-way door.
- **2026-09-01 — G0 bulk stamp** `[DECIDED]`: sponsor ruling (verbatim, post-compaction message): "if you don't have any questions, you can take my stamp and apply G0 then start phase 1 kickoff." The orchestrator had presented the §9 defaults in bulk before compaction and had no questions; every default is adopted as recommended:
  - **Q1 License:** MIT OR Apache-2.0 dual. Two-way door until the first external contribution.
  - **Q2 Spec home:** the full corpus is committed at `docs/spec/` (this file, `01-scenarios.md`, later kickoff specs at `9N-phaseN-kickoff.md`); `z_ignore/` keeps only the session handoff. Two-way door.
  - **Q3 Ack semantics:** local ack + explicit jump-to-agent (`agent.focus`), because herdr has no mark-seen API (§5.7). Two-way door; Phase 4/5 circle-back.
  - **Q4 Launch outside a configured parent dir:** watch the launch cwd ad hoc with a notice; never error. Two-way door.
  - **Q5 Accept-all confirmation:** confirm only when more than 10 files are affected; threshold hardcoded in v1. Two-way door; Phase 4 circle-back.
  - **Q6 Debounce:** 750 ms, hardcoded in v1 (no config key). Two-way door; Phase 4 circle-back.
  - **Q8 Agent-status strategy:** per-pane `pane.agent_status_changed` subscriptions + resync is the §5.4 contract. One-way-ish: the client topology is built around it; revisit only if herdr ships a global status stream.
  - **Editorial fixes made while stamping** (orchestrator, no semantic change): `[herdr]` became a TOML table with `mode`/`session` (§6.1); fixture repos are script-generated rather than vendored tarballs (§8 Phase 1); Phase 1's CI gate item carries its split interpretation; toolchain and herdr-fetch facts recorded in §4.1/§4.4; `lastcall-testkit` named as the third crate (§4.1).
  - **§11 deferral ledger:** the seven seeded candidates are confirmed as standing entries under this stamp.
- **2026-09-01 — Phase 1 rulings** (presented at close-out; each `[LEANING]` becomes `[DECIDED]` when the sponsor merges the Phase 1 PR or replies otherwise):
  - **Protocol 20 vs 21** `[DECIDED]` (sponsor, 2026-09-01, verbatim: "rec plz", after a full rec-vs-alt impact walk-through): accept both (Amendment v1.1) rather than pin 20 only — the consumed surface is byte-identical on both and the next herdr release will ship 21; the engine never depends on the socket, so the worst case is degraded herdr extras, never review correctness. Alternative on record: pin 20 and bump at the Phase 5 circle-back. **Attached roadmap amendment (§3.4):** the scheduled herdr-compat workflow moves from Phase 9 to Phase 5, so a 21 release is diffed against our consumed-surface fixture within a day of shipping — this closes rec's one blind spot (a herdr upgrade on the sponsor's machine ahead of our pin). Two-way door.
  - **Status-connection teardown on agent release** `[LEANING]`: accept the worker's additive extension (Amendment v1.1 item 3). Alternative: keep the stream for the pane's lifetime. Two-way door.
  - **Informational, no ruling needed:** the worker fast-forwarded its harness-created branch from `main` onto `feat/phase1-scaffold` (pure ff, verified); per-pane generation counters (a `pane.get` invalidates only that pane's in-flight status events); `hello-herdr --socket` exits 0 on peer disconnect (documented); XDG resolution is hand-rolled through the injected `Env`.
  - **Phase 2 unit-test floor: 101** (engine 85, testkit 16, bin 0), recorded here; ratchets only upward.
- **2026-09-01 — Phase 2 kickoff rulings** `[DECIDED]` (sponsor, verbatim: "1 rec 2 explain more 3 rec 4 explain more", then "rec on both"):
  - **D4 case-only rename on macOS:** keep the frozen scenario and satisfy it mechanically — on a case-insensitive root the current-side existence of an index/override path is a byte-exact `readdir` of its parent (cached per scan), and `ls-files --others` runs with `-c core.ignorecase=false`; result `f.txt` deleted + `F.txt` added for both root kinds. Alternative on record: draft roots show the addition only and D4 is amended. Two-way door.
  - **Draft-glob semantics:** a relative `draft_dirs` entry matches directories under each configured parent dir **and** under each discovered git root; the matched directory is the root; its untracked files leave the enclosing git root's candidates, tracked content stays the git root's. Alternative: parent dirs only. Two-way door.
  - **`lastcall watch` subcommand** is in Phase 2 scope as the sponsor-visible surface for transition notices (`--exit-after`, and `--poll` added during review for hosts without filesystem events). Alternative: `status` only. Two-way door.
  - **Unanchored store objects:** git never physically writes an object the alternate already has, so the seen tree lives in the user's objects dir and can be pruned after a history rewrite plus reflog expiry; ruling = fail open (a missing seen tree is `null` for the session with a notice; the next accept-all refolds from empty), never `gc`/`prune`/`repack` the store, hardening deferred to §11. Alternative: copy tree objects into the store at first sight and every fold (~a day of scope, fragile alternate toggling). Two-way door.
- **2026-09-02 — Phase 2 close-out** (each `[LEANING]` becomes `[DECIDED]` when the sponsor merges PR #2 or replies otherwise):
  - **Unreadable or foreign ledger opens with nothing seen** `[LEANING]`: an unparsable ledger, an unknown schema major, or a ledger whose recorded `root` is not the opened root is moved aside (`ledger.json.unreadable-<secs>-<n>`) and the root opens with `seen_tree = null` (everything pending, invariant 2) — deliberately *not* first sight at the current HEAD, which would hide every commit since the original first sight. Alternative: first sight. Two-way door.
  - **`open` reads a present ledger without the lock** `[LEANING]`: `status` never blocks behind a long fold; writes still lock, re-load and merge. Alternative: lock-first open with a longer retry. Two-way door.
  - **`watch` publishes a pile per root per scan even when unchanged** `[LEANING]`: per the kickoff ("Pile after every scan"); Phase 3 may publish only on change. Two-way door.
  - **Informational, no ruling needed:** D6 hides a *deleted* skip-worktree file (git semantics; the filter can only suppress absent paths); the B8 best-effort "stashed" notice is not implemented (reset/checkout notices cover the observable transitions); `--poll <secs>` sets both polling backstops; worker spend projected above the $3 flag and below the $5 stop (two verifier passes plus a filesystem-event investigation; no meter available to the worker).
  - **Phase 3 unit-test floor: 182** (engine 166, testkit 16, bin 0), recorded here; ratchets only upward.

## 11. Deferral ledger

(standing; entries: deferral, why acceptable now, hardening shape, trigger condition, decided-where)

Standing entries, confirmed at G0 on 2026-09-01 (§10):
- **Event-loss blindness accepted** (no wire sequence numbers): acceptable because invariant 9's resync heals it within 30 s; hardening = shorter fallback or a herdr upstream feature request; trigger = any observed stale-state report in real use.
- **herdr `done` granularity is per-tab, not per-pane:** acceptable at the design workload (one agent per tab in practice); trigger = multi-agent-per-tab workflows becoming common.
- **Externally created worktrees discovered only by rescan** (herdr emits no events for them, §5.8): acceptable because the periodic rescan bounds staleness to its interval; hardening = shorter interval or watching repo `.git/worktrees/` dirs; trigger = agent-created worktrees routinely missed during live review.
- **Restore's residual hash-then-rename window** (§6.3; review F7): acceptable because the window is microseconds, the post-restore rescan surfaces any delta that lands in it as pending (fail-open), and true cross-process locking against arbitrary agents is not available; hardening = advisory lock honored by cooperating tools, or a restore that writes only when the file's mtime+size are unchanged across the rename; trigger = any observed lost agent write attributed to a restore.
- **Alternates read-miss** (§6.1): our private store references unchanged tracked blobs in the user's repo via `objects/info/alternates`; an object that exists in the user's repo only as an unreachable loose object could be pruned by their `git gc`, after which our reference fails open (that path re-flags in full). Acceptable because objects reachable from any of their refs are never pruned and everything else we wrote ourselves; hardening = copy-on-reference for objects not reachable from HEAD; trigger = any observed spurious re-flag traced to a missing object.
- **Upstream annotation is a heuristic** (§6.4): "reachable from a remote-tracking ref" misclassifies a branch an agent pushed from *another* machine as upstream — it is grouped, not hidden, so the cost is one un-group click; hardening = also require author ≠ `user.email` or a configurable remote allowlist; trigger = the group row routinely containing agent work.
- **No per-branch seen trees** (§6.4): hopping to a never-reviewed branch over-shows its whole delta until you hop back; acceptable at the design workload (agents work in worktrees; in-place hops are brief); hardening = `seen_tree_by_branch` map in the ledger (additive schema change); trigger = in-place branch hopping becoming a daily pattern.
- **v0.8.2 lifecycle-subscription replay on connect** (Amendment v1.1): every connect/reconnect replays up to 512 stale lifecycle events over ~50 s, costing coalesced extra snapshots and short-lived `pane_not_found` status subscriptions; acceptable because events are hints and the cost is bounded; hardening = none needed once the pinned release includes herdr's cursor-snapshot fix (master `5158ada`); trigger = a pinned release ≥ that fix (drop the entry) or observed connect-time sluggishness.
- **Title-change `pane_updated` events drive a snapshot per command** (observed in the Phase 1 sponsor demo, 2026-09-01): herdr emits `pane_updated` whenever a pane's terminal title changes, i.e. on every command a user runs in any pane, and the client schedules a (coalesced) `session.snapshot` for each; acceptable at the design workload (one snapshot per 500 ms at most, and the sponsor demo stayed responsive); hardening = ignore a `pane_updated` whose only delta versus the cached record is `title`/`terminal_title*`/`revision`, or resync that pane with `pane.get` instead of a full snapshot; trigger = visible lag or herdr CPU attributable to snapshot frequency, or Phase 5's UI wiring (do it then).
- **Unanchored seen trees, not only loose blobs** (Phase 2 design review F1, extends "Alternates read-miss"): git skips the physical write for any object the alternate already has, so `HEAD^{tree}` at first sight and any fold result equal to an existing tree live only in the user's objects dir; a history rewrite + reflog expiry (30 d) + auto-gc deletes them and `read-tree <seen_tree>` fails. Acceptable because the engine checks `exists(seen_tree)` on open and before every fold and fails open (`null` seen tree, notice, refold from empty); hardening = `refs/lastcall/seen` and an overrides-tree ref in the store plus copying the tree objects in, after which store `gc` also becomes safe; trigger = any observed "seen tree missing" notice in real use.
- **Private store grows without bound and is never gc'd** (§6.1, Phase 2): one loose object per scanned version of each changed file and per accepted hunk; nothing in the store is ref-anchored, so `gc`/`prune`/`repack -d` are forbidden. Acceptable at the design workload (objects are small, alternates cover unchanged content); hardening = the ref anchors above, then `git gc --auto` on the store; trigger = a state dir above ~1 GB or a sponsor complaint.
- **Non-UTF-8 paths are pending but not acceptable** (Phase 2): paths are bytes end to end and JSON renders them lossily; a non-UTF-8 path shows as a row with a notice and every accept refuses it (over-show). Acceptable because APFS refuses to create such names and Linux repos with them are rare; hardening = base64 or `\x` escaping in the JSON and a byte-path accept API; trigger = a user report.
- **Clean filters run inside the private store** (Phase 2 design review F18): `hash-object -w` under `GIT_DIR=<store>` executes the user's clean filters (`filter=lfs` runs `git-lfs clean`, which writes under `<store>/lfs/` or fails when git-lfs is absent — the path then shows as `unreadable`). Acceptable because the result is over-show; hardening = `-c filter.<name>.required=false` plus a notice, or treating LFS pointers as collapsed; trigger = LFS-backed repos in real use.
- **Watcher event delivery is proven only where FSEvents works** (Phase 2): the polling backstops (HEAD 10 s, rescan 30 s, `--poll`) carry a host whose `fseventsd` is unhealthy; the FS-delivery integration test self-skips with a visible reason there. Acceptable because invariant 9 makes events hints; hardening = none needed; trigger = a Phase 3 UI feeling sluggish on a healthy host.
- **Atomic-rename restore orphans open file descriptors** (§6.3): an editor/agent holding the old inode open keeps writing to the orphan and its later writes vanish; acceptable because agents write-and-close and the rescan shows the restore result honestly; hardening = in-place write for files with open writers (detect via `lsof`/`fuser`); trigger = a user report of an editor buffer diverging after restore.

## Amendments

(log entries take the form: version, date, phase N, change, reason it was unknowable at design time)

- **v1.1 — 2026-09-01 — Phase 1 — §5 herdr surface — PROPOSED; ratified by the sponsor merging the Phase 1 PR.** Changes: (1) §5.2: the released v0.8.2 answers protocol **20**, not 21; the client accepts `[20, 21]`; §4.6 range updated. (2) §5.3: on v0.8.2, lifecycle subscriptions replay the whole event ring on connect (sequence 0 start); per-pane subscriptions do not; the client treats the replay burst as hints only and never derives the review-ready indicator from a replayed lifecycle event. (3) §5.4/§6.6, additive: a per-pane status connection is also closed at resync when its pane is no longer agent-bearing (agent released), not only on `pane_closed`/`pane_exited` — keeps the "one connection per agent-bearing pane" sizing honest. Why unknowable at design time: §5 was verified against master `5158ada`, believed to equal the v0.8.2 release because `Cargo.toml` still said 0.8.2; the tag was 55 commits behind master and the difference only surfaced when the worker's integration test ran the published binary. Standing rule from this: **every §5 claim is verified against the release tag we pin, never master**; the Phase 5 circle-back re-verifies against the then-pinned tag.
- **v1.2 — 2026-09-02 — Phase 2 — §6.1/§6.4/§6.5 editorial and additive — PROPOSED; ratified by the sponsor merging the Phase 2 PR.** Changes: (1) §6.5: pending, conflict and in-progress state are computed from the private index, `ls-files -u`, and the git-dir files; `git status` (and `diff`, `add`, `checkout`, `stash`, anything that can take the user's `index.lock`) is **never run** on the user's repository — porcelain v2 remains the reference *semantics* only. (2) §6.1: the private store is never `gc`'d, pruned or repacked (nothing in it is ref-anchored); an orphan object from a crash is simply left in place; an unresolvable seen tree opens as `null` (fail open). (3) §6.1, additive files under `repos/<repo-hash>/`: `index.tree` (the tree the private index was seeded from), `index.tmp` (the fold's temp index), `lock` (advisory `flock`; every ledger read-modify-write holds it, re-reads, then writes tmp + fsync + rename). (4) §6.1 config keys copied into the store at every open now include `info/attributes`, and the store forces `core.fsmonitor=false`, `core.untrackedCache=false`, `core.splitIndex=false` (a user's global `core.fsmonitor=true` otherwise hangs the private-index refresh). (5) §6.4: remote-tracking refs are a classification input — a fetch or push may change `upstream`/`mixed` labels without HEAD moving. (6) §6.5, draft roots: relative `draft_dirs` globs match under parent dirs and under discovered git roots (§10 ruling). (7) New surfaces: `lastcall status [--json] [--root <path>…]` emits `status_version: 1` (schema in `docs/dev/engine.md`; an unresolvable `--root` exits 1); `lastcall watch [--json] [--exit-after <secs>] [--poll <secs>]` streams engine events. (8) §6.4 D6: a skip-worktree path that is absent from disk is never a deletion; one that is present is a normal candidate. Why unknowable at design time: §6.1/§6.5 were drafted before the harness and the build showed that git skips writes for alternate-resident objects, that the store inherits the user's global config (fsmonitor), and that `git status` on the user's repo is both unnecessary and a lock hazard.

# Provenance: consumed-surface.json

**Generated from the pinned herdr release tag `v0.9.0`** (the release asset fetched by `just
herdr-fetch`, which verifies `herdr --version` == `herdr 0.9.0`), never from `master` and never
by hand.

Exact command:

```
just herdr-schema-fixture
```

which runs

```
cargo run -q -p lastcall-testkit --example herdr_schema_fixture -- \
    target/herdr/v0.9.0/herdr \
    crates/lastcall-testkit/fixtures/herdr/schema/consumed-surface.json
```

Generated on **2026-09-11** (Phase 9b deliverable 14 moved the pin v0.8.2 -> v0.9.0). The
generator reported:

```
herdr-schema-fixture: herdr 0.9.0, protocol 22, schema_version 1 -> …/consumed-surface.json
  (52000 bytes, 11 methods, 15 events, 46 defs)
```

The diff against the v0.8.2 projection is **additive only**, and none of the four additions is
a field lastcall reads: `request/WorktreeListParams.trust_repository`,
`request/ServerCapabilities.{endpoint_protocol_generation, health_check, surface_interest}`,
and `protocol` 20 -> 22. Method, event, result and def counts are unchanged (11 / 15 / 11 /
46), so no consumed type gained, lost or changed a member.

`herdr api schema --json` reads no socket and starts no server, so generating this file touches
nothing outside the repo.

## What it is

A **projection** of `herdr api schema --json` (255 KB, 91 request variants, 58 result variants)
onto the surface lastcall actually consumes, built by
`crates/lastcall-testkit/src/herdr_schema.rs`. Pinning the whole schema would make every
unrelated herdr feature a drift alert, and an alert nobody trusts is not a check.

| key | contents |
|---|---|
| `protocol`, `schema_version` | `22`, `1` — a bump in either is drift by itself |
| `methods` | the eleven methods we call: params schema, the request variant's `required`, and the `type` const of the result our code deserializes |
| `results` | those eleven result variants |
| `events` | the §5.4 lifecycle set (15), keyed by the dotted **subscription** name, each recording the snake_case `event` name herdr pushes and its payload schema |
| `subscription_events` | `pane.agent_status_changed` (§5.5's second envelope shape) |
| `pinned_enums` | `AgentStatus` and `NotificationShowSound` — the two vocabularies our code branches on |
| `defs` | every type those reach transitively, keyed `<section>/<Name>` |

The method → result mapping is not derivable from the schema (herdr's `ResponseResult` is one
flat `oneOf` with no link back to a method), so `CONSUMED_METHODS` states it and the generator
*verifies* each named result const exists. Each entry was read off herdr v0.8.2's own handlers:
`agent.focus` → `agent_info` (`src/app/api/agents.rs:35-42`), `workspace.get` → `workspace_info`
(`src/app/api/workspaces.rs:23-35`), `tab.focus` → `tab_info` (`src/app/api/tabs.rs:132-140`),
`pane.send_text` → `ok` (Phase 7 deliverable 6 — the schema's shared no-payload success const,
confirmed present in `ResponseResult` by the generator); the other seven were verified in
Phase 1's §5 pass.

`workspace.get` and `tab.focus` are consumed by the real-server tests rather than by the client.
They are pinned because a change to either breaks those tests.

`$ref`s are rewritten from `#/schemas/<section>/$defs/<Name>` to `#/defs/<section>/<Name>`. The
section stays in the key deliberately: herdr defines `PaneInfo`, `AgentStatus`, `TabInfo` and
others separately under `request`, `event`, `subscription_event` and `success_response`, and a
projection that merged them would hide the day one of them changes alone. `AgentStatus` is the
one exception where sameness is asserted rather than assumed — the generator fails if the four
sections disagree, because our code assumes they do not.

Determinism: `serde_json::Map` is a `BTreeMap` in this workspace (no `preserve_order` feature in
`Cargo.lock`), so every object is key-sorted; the writer appends exactly one `\n`.

## Who reads it

- `herdr_real_schema_consumed_surface_unchanged` in
  `crates/lastcall-engine/tests/test_integration_herdr_real.rs` — projects the schema of the
  binary under test and compares, printing a path-by-path diff on mismatch.
- `.github/workflows/herdr-compat.yml` — weekly (Monday 06:00 UTC), against the **latest** herdr
  release rather than the pinned tag, so drift is found before we adopt it.

## Regenerating

When the pin moves, or when `CONSUMED_METHODS` gains a method. Bump `herdr_version` in the
`justfile` if the pin is what moved, run `just herdr-schema-fixture`, update the tag, date and
counts above, and re-read the diff: this file is the
record of what we believe herdr's API is, so a diff nobody explained is a diff nobody checked.

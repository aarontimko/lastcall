# Provenance: consumed-surface.json

**Generated from the pinned herdr release tag `v0.8.2`** (the release asset fetched by `just
herdr-fetch`, which verifies `herdr --version` == `herdr 0.8.2`), never from `master` and never
by hand.

Exact command:

```
just herdr-schema-fixture
```

which runs

```
cargo run -q -p lastcall-testkit --example herdr_schema_fixture -- \
    target/herdr/v0.8.2/herdr \
    crates/lastcall-testkit/fixtures/herdr/schema/consumed-surface.json
```

Generated on **2026-09-04**. The generator reported:

```
herdr-schema-fixture: herdr 0.8.2, protocol 20, schema_version 1 -> …/consumed-surface.json
  (50622 bytes, 10 methods, 15 events, 45 defs)
```

`herdr api schema --json` reads no socket and starts no server, so generating this file touches
nothing outside the repo.

## What it is

A **projection** of `herdr api schema --json` (255 KB, 91 request variants, 58 result variants)
onto the surface lastcall actually consumes, built by
`crates/lastcall-testkit/src/herdr_schema.rs`. Pinning the whole schema would make every
unrelated herdr feature a drift alert, and an alert nobody trusts is not a check.

| key | contents |
|---|---|
| `protocol`, `schema_version` | `20`, `1` — a bump in either is drift by itself |
| `methods` | the ten methods we call: params schema, the request variant's `required`, and the `type` const of the result our code deserializes |
| `results` | those ten result variants |
| `events` | the §5.4 lifecycle set (15), keyed by the dotted **subscription** name, each recording the snake_case `event` name herdr pushes and its payload schema |
| `subscription_events` | `pane.agent_status_changed` (§5.5's second envelope shape) |
| `pinned_enums` | `AgentStatus` and `NotificationShowSound` — the two vocabularies our code branches on |
| `defs` | every type those reach transitively, keyed `<section>/<Name>` |

The method → result mapping is not derivable from the schema (herdr's `ResponseResult` is one
flat `oneOf` with no link back to a method), so `CONSUMED_METHODS` states it and the generator
*verifies* each named result const exists. Each entry was read off herdr v0.8.2's own handlers:
`agent.focus` → `agent_info` (`src/app/api/agents.rs:35-42`), `workspace.get` → `workspace_info`
(`src/app/api/workspaces.rs:23-35`), `tab.focus` → `tab_info` (`src/app/api/tabs.rs:132-140`);
the other seven were verified in Phase 1's §5 pass.

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

Only when the pin moves. Bump `herdr_version` in the `justfile`, run `just
herdr-schema-fixture`, update the tag and date above, and re-read the diff: this file is the
record of what we believe herdr's API is, so a diff nobody explained is a diff nobody checked.

//! The **consumed surface** projection of `herdr api schema --json` (kickoff deliverable 11a).
//!
//! herdr's own schema is ~250 KB and 91 request variants wide; almost none of it is ours.
//! Diffing the whole thing against a release would flag every unrelated feature herdr ships,
//! and a drift check that cries wolf is a drift check nobody reads. So the fixture pins a
//! *projection*: the ten methods we call, the §5.4 lifecycle event set plus the one per-pane
//! subscription event, every type schema those transitively reach, and the two enums whose
//! values our code branches on.
//!
//! The projection is deterministic by construction: `serde_json::Map` is a `BTreeMap` in this
//! workspace (no `preserve_order` feature anywhere in `Cargo.lock`), so every object it writes
//! is key-sorted, and the writer appends a single `\n`.
//!
//! `$ref`s are rewritten from herdr's `#/schemas/<section>/$defs/<Name>` to `#/defs/<section>/<Name>`
//! and the targets collected under `defs`. The section stays in the key on purpose: herdr
//! defines `PaneInfo` separately under `request`, `event` and `success_response`, and a
//! projection that merged them would hide the day one of them changes alone.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

/// The methods we call, each with the `type` const of the result our code deserializes.
///
/// The result const is not derivable from the schema — herdr's `ResponseResult` is one flat
/// `oneOf` with no link back to the method — so it is stated here and **verified** against the
/// schema by [`project`]. Each was read off herdr v0.8.2's own handlers:
/// `ping`/`session.snapshot`/`pane.get`/`pane.list`/`worktree.list`/`events.subscribe`/
/// `notification.show` from Phase 1's §5 verification, `agent.focus` → `agent_info`
/// (`src/app/api/agents.rs:35-42`), `workspace.get` → `workspace_info`
/// (`src/app/api/workspaces.rs:23-35`), `tab.focus` → `tab_info` (`src/app/api/tabs.rs:132-140`).
///
/// `workspace.get` and `tab.focus` are consumed by the real-server tests rather than by the
/// client; they are in the kickoff's list because a change to either breaks those tests.
pub const CONSUMED_METHODS: [(&str, &str); 10] = [
    ("agent.focus", "agent_info"),
    ("events.subscribe", "subscription_started"),
    ("notification.show", "notification_show"),
    ("pane.get", "pane_info"),
    ("pane.list", "pane_list"),
    ("ping", "pong"),
    ("session.snapshot", "session_snapshot"),
    ("tab.focus", "tab_info"),
    ("worktree.list", "worktree_list"),
    ("workspace.get", "workspace_info"),
];

/// The §5.4 lifecycle set, as the **subscription** names we send.
///
/// Kept literal rather than imported from `lastcall_engine::herdr::wire` so that a change to
/// our own constant shows up as a fixture diff instead of silently re-pinning a different set.
pub const CONSUMED_LIFECYCLE_EVENTS: [&str; 15] = [
    "pane.agent_detected",
    "pane.closed",
    "pane.created",
    "pane.exited",
    "pane.focused",
    "pane.moved",
    "pane.updated",
    "tab.focused",
    "workspace.closed",
    "workspace.created",
    "workspace.focused",
    "workspace.updated",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
];

/// The per-pane subscription event (§5.5's second envelope shape).
pub const CONSUMED_SUBSCRIPTION_EVENTS: [&str; 1] = ["pane.agent_status_changed"];

/// Enums whose *values* our code branches on, pinned by name so a new or renamed variant is a
/// diff even when the referencing schema is untouched.
///
/// `AgentStatus` drives every dot and rollup (§6.6); `NotificationShowSound` is the tag we send
/// as `sound` — our engine type is `Option<String>` and stays lenient, the fixture is what
/// notices herdr changing the vocabulary.
pub const PINNED_ENUMS: [(&str, &str); 2] = [
    ("AgentStatus", "event"),
    ("NotificationShowSound", "request"),
];

/// A section of herdr's schema (`schemas.<section>`).
const SECTIONS: [&str; 4] = ["event", "request", "subscription_event", "success_response"];

/// Project `schema` (the parsed output of `herdr api schema --json`) onto the consumed surface.
pub fn project(schema: &Value) -> Result<Value, String> {
    let protocol = schema
        .get("protocol")
        .ok_or("schema has no `protocol`")?
        .clone();
    let schema_version = schema
        .get("schema_version")
        .ok_or("schema has no `schema_version`")?
        .clone();

    let mut wanted: BTreeSet<(String, String)> = BTreeSet::new();

    // Methods: the request variant whose `method` const matches, plus the result variant whose
    // `type` const is the one our code deserializes.
    let request_variants = one_of(schema, "request", &["$defs", "RequestBody"])
        .or_else(|_| top_one_of(schema, "request"))?;
    let result_variants = one_of(schema, "success_response", &["$defs", "ResponseResult"])?;

    let mut methods = Map::new();
    let mut results = Map::new();
    for (method, result_type) in CONSUMED_METHODS {
        let variant = find_const(request_variants, "method", method)
            .ok_or_else(|| format!("no request variant for method `{method}`"))?;
        let params = variant
            .get("properties")
            .and_then(|p| p.get("params"))
            .ok_or_else(|| format!("request variant for `{method}` has no `params`"))?;
        methods.insert(
            method.to_string(),
            json!({
                "params": rewrite(params, &mut wanted),
                "required": variant.get("required").cloned().unwrap_or(Value::Null),
                "result": result_type,
            }),
        );

        let result = find_const(result_variants, "type", result_type).ok_or_else(|| {
            format!("no result variant `{result_type}` (the result of `{method}`)")
        })?;
        results.insert(result_type.to_string(), rewrite(result, &mut wanted));
    }

    // Lifecycle events: subscription names are dotted, the pushed `event` field is snake_case
    // (§5.5). Both are recorded, so a rename on either side is a diff.
    let event_kinds = enum_values(schema, "event", "EventKind")?;
    let event_variants = one_of(schema, "event", &["$defs", "EventData"])?;
    let mut events = Map::new();
    for subscription in CONSUMED_LIFECYCLE_EVENTS {
        let wire_name = subscription.replace('.', "_");
        if !event_kinds.contains(&wire_name) {
            return Err(format!(
                "`{subscription}` maps to event `{wire_name}`, which EventKind does not list"
            ));
        }
        let variant = find_const(event_variants, "type", &wire_name)
            .ok_or_else(|| format!("no EventData variant `{wire_name}`"))?;
        events.insert(
            subscription.to_string(),
            json!({ "event": wire_name, "data": rewrite(variant, &mut wanted) }),
        );
    }

    // Subscription events keep their dotted name on the wire.
    let subscription_kinds = enum_values(schema, "subscription_event", "SubscriptionEventKind")?;
    let mut subscription_events = Map::new();
    for name in CONSUMED_SUBSCRIPTION_EVENTS {
        if !subscription_kinds.contains(&name.to_string()) {
            return Err(format!("SubscriptionEventKind does not list `{name}`"));
        }
        // The payload type is named for the event: `pane.agent_status_changed` →
        // `PaneAgentStatusChangedEvent`.
        let def_name = format!("{}Event", upper_camel(name));
        let def = def(schema, "subscription_event", &def_name)
            .ok_or_else(|| format!("subscription_event has no `{def_name}`"))?;
        subscription_events.insert(
            name.to_string(),
            json!({ "event": name, "data": rewrite(def, &mut wanted) }),
        );
    }

    // Pinned enums, and a cross-section equality check: herdr repeats `AgentStatus` in every
    // section, and our code assumes the four agree.
    let mut pinned = Map::new();
    for (name, section) in PINNED_ENUMS {
        let values = enum_values(schema, section, name)?;
        for other in SECTIONS {
            if other == section {
                continue;
            }
            if let Ok(also) = enum_values(schema, other, name)
                && also != values
            {
                return Err(format!(
                    "`{name}` differs between `{section}` and `{other}`: {values:?} vs {also:?}"
                ));
            }
        }
        pinned.insert(
            name.to_string(),
            Value::Array(values.into_iter().map(Value::String).collect()),
        );
    }

    // Everything the above reaches, transitively.
    let mut defs: BTreeMap<String, Value> = BTreeMap::new();
    while let Some((section, name)) = wanted.iter().next().cloned() {
        wanted.remove(&(section.clone(), name.clone()));
        let key = format!("{section}/{name}");
        if defs.contains_key(&key) {
            continue;
        }
        let body = def(schema, &section, &name)
            .ok_or_else(|| format!("dangling $ref: schemas/{section}/$defs/{name}"))?;
        defs.insert(key, rewrite(body, &mut wanted));
    }

    Ok(json!({
        "protocol": protocol,
        "schema_version": schema_version,
        "methods": Value::Object(methods),
        "results": Value::Object(results),
        "events": Value::Object(events),
        "subscription_events": Value::Object(subscription_events),
        "pinned_enums": Value::Object(pinned),
        "defs": defs.into_iter().collect::<Map<_, _>>(),
    }))
}

/// Serialize a projection the one way the fixture is written: pretty, key-sorted (a
/// `serde_json::Map` is a `BTreeMap` here), one trailing newline.
pub fn render(projection: &Value) -> String {
    let mut out = serde_json::to_string_pretty(projection).unwrap_or_default();
    out.push('\n');
    out
}

/// A readable diff of two projections: the paths that differ, one per line.
///
/// `serde_json`'s `assert_eq!` output on a 100 KB value is unreadable, and the whole point of
/// the compat job is that a human reads its failure.
pub fn diff(expected: &Value, actual: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_diff("", expected, actual, &mut out);
    out
}

fn walk_diff(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let child = format!("{path}/{key}");
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => walk_diff(&child, x, y, out),
                    (Some(_), None) => out.push(format!("{child}: removed by this herdr")),
                    (None, Some(_)) => out.push(format!("{child}: added by this herdr")),
                    (None, None) => {}
                }
            }
        }
        (a, b) if a == b => {}
        (a, b) => out.push(format!(
            "{path}: fixture {} but this herdr {}",
            compact(a),
            compact(b)
        )),
    }
}

fn compact(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 160 {
        format!("{}…", &text[..160])
    } else {
        text
    }
}

/// Rewrite `#/schemas/<section>/$defs/<Name>` to `#/defs/<section>/<Name>`, recording each
/// target in `wanted`.
fn rewrite(value: &Value, wanted: &mut BTreeSet<(String, String)>) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if key == "$ref"
                    && let Some(text) = child.as_str()
                    && let Some((section, name)) = parse_ref(text)
                {
                    wanted.insert((section.clone(), name.clone()));
                    out.insert(
                        key.clone(),
                        Value::String(format!("#/defs/{section}/{name}")),
                    );
                    continue;
                }
                out.insert(key.clone(), rewrite(child, wanted));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| rewrite(i, wanted)).collect()),
        other => other.clone(),
    }
}

fn parse_ref(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("#/schemas/")?;
    let (section, rest) = rest.split_once('/')?;
    let name = rest.strip_prefix("$defs/")?;
    Some((section.to_string(), name.to_string()))
}

fn def<'a>(schema: &'a Value, section: &str, name: &str) -> Option<&'a Value> {
    schema.get("schemas")?.get(section)?.get("$defs")?.get(name)
}

fn enum_values(schema: &Value, section: &str, name: &str) -> Result<Vec<String>, String> {
    let body =
        def(schema, section, name).ok_or_else(|| format!("schemas/{section}/$defs/{name}"))?;
    let values = body
        .get("enum")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("schemas/{section}/$defs/{name} is not an enum"))?;
    values
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("schemas/{section}/$defs/{name} has a non-string variant"))
        })
        .collect()
}

fn one_of<'a>(schema: &'a Value, section: &str, path: &[&str]) -> Result<&'a Value, String> {
    let mut node = schema
        .get("schemas")
        .and_then(|s| s.get(section))
        .ok_or_else(|| format!("no schemas/{section}"))?;
    for step in path {
        node = node
            .get(step)
            .ok_or_else(|| format!("no schemas/{section}/{}", path.join("/")))?;
    }
    node.get("oneOf")
        .ok_or_else(|| format!("schemas/{section}/{} has no oneOf", path.join("/")))
}

fn top_one_of<'a>(schema: &'a Value, section: &str) -> Result<&'a Value, String> {
    schema
        .get("schemas")
        .and_then(|s| s.get(section))
        .and_then(|s| s.get("oneOf"))
        .ok_or_else(|| format!("schemas/{section} has no oneOf"))
}

/// The `oneOf` member whose `properties.<field>.const` equals `value`.
fn find_const<'a>(variants: &'a Value, field: &str, value: &str) -> Option<&'a Value> {
    variants.as_array()?.iter().find(|variant| {
        variant
            .get("properties")
            .and_then(|p| p.get(field))
            .and_then(|f| f.get("const"))
            .and_then(Value::as_str)
            == Some(value)
    })
}

/// `pane.agent_status_changed` → `PaneAgentStatusChanged`.
fn upper_camel(name: &str) -> String {
    name.split(['.', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature schema in herdr's shape: enough for the projection to have something to do.
    fn tiny() -> Value {
        json!({
            "protocol": 20,
            "schema_version": 1,
            "schemas": {
                "request": {
                    "oneOf": [
                        { "properties": { "method": { "const": "ping" },
                                          "params": { "$ref": "#/schemas/request/$defs/PingParams" } },
                          "required": ["method", "params"] },
                        { "properties": { "method": { "const": "other" },
                                          "params": { "type": "object" } } }
                    ],
                    "$defs": {
                        "PingParams": { "type": "object" },
                        "NotificationShowSound": { "enum": ["none", "done"], "type": "string" }
                    }
                },
                "success_response": {
                    "$defs": {
                        "ResponseResult": { "oneOf": [
                            { "properties": { "type": { "const": "pong" },
                                              "caps": { "$ref": "#/schemas/success_response/$defs/Caps" } } }
                        ] },
                        "Caps": { "properties": { "n": { "type": "integer" } } }
                    }
                },
                "event": {
                    "$defs": {
                        "EventKind": { "enum": ["pane_created"], "type": "string" },
                        "EventData": { "oneOf": [
                            { "properties": { "type": { "const": "pane_created" },
                                              "pane": { "$ref": "#/schemas/event/$defs/PaneInfo" } } }
                        ] },
                        "PaneInfo": { "properties": { "pane_id": { "type": "string" } } },
                        "AgentStatus": { "enum": ["idle", "done"], "type": "string" }
                    }
                },
                "subscription_event": {
                    "$defs": {
                        "SubscriptionEventKind": { "enum": ["pane.agent_status_changed"], "type": "string" },
                        "PaneAgentStatusChangedEvent": {
                            "properties": { "agent_status": { "$ref": "#/schemas/subscription_event/$defs/AgentStatus" } }
                        },
                        "AgentStatus": { "enum": ["idle", "done"], "type": "string" }
                    }
                }
            }
        })
    }

    /// The projection under a consumed set cut down to what `tiny` defines.
    fn project_tiny(schema: &Value) -> Result<Value, String> {
        // `project` reads the real constant lists, so exercise the pieces directly here and
        // keep the whole-surface check for the real-server test (which has the real schema).
        let mut wanted = BTreeSet::new();
        let variants = top_one_of(schema, "request")?;
        let ping = find_const(variants, "method", "ping").ok_or("no ping")?;
        let params = rewrite(
            ping.get("properties").unwrap().get("params").unwrap(),
            &mut wanted,
        );
        let results = one_of(schema, "success_response", &["$defs", "ResponseResult"])?;
        let pong = rewrite(
            find_const(results, "type", "pong").ok_or("no pong")?,
            &mut wanted,
        );
        let mut defs = BTreeMap::new();
        while let Some((section, name)) = wanted.iter().next().cloned() {
            wanted.remove(&(section.clone(), name.clone()));
            let body =
                def(schema, &section, &name).ok_or_else(|| format!("dangling {section}/{name}"))?;
            defs.insert(format!("{section}/{name}"), rewrite(body, &mut wanted));
        }
        Ok(json!({ "params": params, "pong": pong,
                   "defs": defs.into_iter().collect::<Map<_, _>>() }))
    }

    #[test]
    fn herdr_schema_rewrites_refs_and_collects_defs_transitively() {
        let projected = project_tiny(&tiny()).expect("projects");
        assert_eq!(
            projected["params"]["$ref"], "#/defs/request/PingParams",
            "refs are rewritten into the projection's own namespace"
        );
        assert_eq!(
            projected["pong"]["properties"]["caps"]["$ref"],
            "#/defs/success_response/Caps"
        );
        let defs = projected["defs"].as_object().expect("defs object");
        assert!(defs.contains_key("request/PingParams"), "{defs:?}");
        assert!(
            defs.contains_key("success_response/Caps"),
            "a ref inside a pulled-in variant is followed: {defs:?}"
        );
        assert_eq!(
            defs.len(),
            2,
            "and nothing herdr defines that we never reach"
        );
    }

    #[test]
    fn herdr_schema_keeps_same_named_types_apart_per_section() {
        // `AgentStatus` exists under both `event` and `subscription_event`; the keys must not
        // collide, or a change to one alone would be invisible.
        let schema = tiny();
        let mut wanted = BTreeSet::new();
        rewrite(
            &json!({ "a": { "$ref": "#/schemas/event/$defs/AgentStatus" },
                     "b": { "$ref": "#/schemas/subscription_event/$defs/AgentStatus" } }),
            &mut wanted,
        );
        assert_eq!(wanted.len(), 2, "{wanted:?}");
        assert!(def(&schema, "event", "AgentStatus").is_some());
        assert!(def(&schema, "subscription_event", "AgentStatus").is_some());
    }

    #[test]
    fn herdr_schema_names_the_event_payload_type_from_the_event_name() {
        assert_eq!(
            upper_camel("pane.agent_status_changed"),
            "PaneAgentStatusChanged"
        );
        assert_eq!(upper_camel("ping"), "Ping");
    }

    #[test]
    fn herdr_schema_diff_names_the_paths_that_moved() {
        let before = json!({ "protocol": 20, "defs": { "a": { "type": "string" }, "gone": 1 } });
        let after = json!({ "protocol": 21, "defs": { "a": { "type": "integer" }, "new": 2 } });
        let lines = diff(&before, &after);
        assert!(
            lines.iter().any(|d| d.starts_with("/protocol:")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|d| d == "/defs/gone: removed by this herdr"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|d| d == "/defs/new: added by this herdr"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|d| d.starts_with("/defs/a/type: fixture \"string\" but this herdr")),
            "{lines:?}"
        );
        assert!(
            diff(&before, &before).is_empty(),
            "and an unchanged schema produces no lines at all"
        );
    }

    #[test]
    fn herdr_schema_projection_is_byte_stable() {
        let a = render(&project_tiny(&tiny()).expect("projects"));
        let b = render(&project_tiny(&tiny()).expect("projects"));
        assert_eq!(a, b);
        assert!(
            a.ends_with("}\n"),
            "one trailing newline: {:?}",
            &a[a.len() - 4..]
        );
    }

    #[test]
    fn herdr_schema_reports_a_method_it_cannot_find() {
        let mut schema = tiny();
        schema["schemas"]["request"]["oneOf"] = json!([]);
        let err = project(&schema).expect_err("no methods at all");
        assert!(err.contains("no request variant for method"), "{err}");
    }

    #[test]
    fn herdr_schema_consumed_lists_match_the_engines_constants() {
        use lastcall_engine::herdr::wire;
        let mut ours: Vec<&str> = CONSUMED_LIFECYCLE_EVENTS.to_vec();
        ours.sort_unstable();
        let mut theirs: Vec<&str> = wire::LIFECYCLE_SUBSCRIPTIONS.to_vec();
        theirs.sort_unstable();
        assert_eq!(
            ours, theirs,
            "the fixture's lifecycle set must be §5.4's, or the drift check watches the wrong events"
        );
        assert_eq!(
            CONSUMED_SUBSCRIPTION_EVENTS[0],
            wire::PANE_AGENT_STATUS_CHANGED
        );
    }
}

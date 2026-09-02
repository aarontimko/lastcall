# Provenance: subscribe_failure.jsonl

**Recorded from the real pinned herdr (v0.8.2 release asset)** by
`crates/lastcall-engine/tests/test_integration_herdr_real.rs` via `just herdr-record`, copied
verbatim by `just fixtures-sync` from `recorded/subscribe_failure.jsonl`.

The one line herdr writes for `events.subscribe` with `{"type":"bogus.event"}` in the set,
after which it closes the connection (spec §5.3 step 1: no partial subscriptions; the
integration test asserts EOF right after this line). Two facts a schema reading would not have
given: the code is `invalid_request` (not `invalid_params`) and the echoed `id` is `""`.
Error codes are plain strings (§5.1); the client handles unknown codes as a generic failure.

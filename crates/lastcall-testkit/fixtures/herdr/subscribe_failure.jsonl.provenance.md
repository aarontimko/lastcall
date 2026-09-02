# Provenance: subscribe_failure.jsonl

Hand-written from the herdr v0.8.2 schema (`docs/next/api/herdr-api.schema.json`, commit `5158ada`):
the error response envelope (`{"id","error":{"code","message"}}`, spec §5.1). Error codes are
plain strings, not a schema enum; `invalid_params` is the code herdr uses when the request
params fail to deserialize (an unknown subscription `type`).

Semantics under test (spec §5.3 step 1): when any subscription in the set fails to construct
the server sends exactly this one error line and closes the connection — there are no partial
subscriptions. The mock replays this line and closes.

Recording status: `just herdr-record` captures the real error line for a bogus subscription set
into `target/herdr-recordings/`; see `recorded/` once recorded.

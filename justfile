# lastcall task runner. Every CI step and every gate command is a target here.
# New tooling goes into a target rather than an ad-hoc command: the committed permission
# allowlist (.claude/settings.json) covers `just …` and little else.

set shell := ["bash", "-euo", "pipefail", "-c"]

# rustup's proxies must win over any stale system cargo, even in shells whose PATH
# predates the rustup install (agent harness shells snapshot PATH at session start).
export PATH := (if path_exists("/opt/homebrew/opt/rustup/bin") == "true" { "/opt/homebrew/opt/rustup/bin:" } else { "" }) + env("HOME") / ".cargo/bin:" + env("PATH")

# The pinned herdr release used by the real-herdr integration tests (docs/spec/00-spec.md §4.4).
# The only sanctioned network fetch in the repo besides cargo's registry and rustup.
herdr_version := "v0.8.2"
herdr_bin := "target/herdr" / herdr_version / "herdr"

# Run any cargo command with the pinned toolchain on PATH: `just cargo add serde`
cargo *ARGS:
    cargo {{ARGS}}

# Print the toolchain the targets will use.
toolchain:
    which cargo && cargo --version && rustc --version

# ---------------------------------------------------------------------------------------
# Build, lint, test (the canonical gate commands)
# ---------------------------------------------------------------------------------------

# Build every crate and every target (lib, bins, tests, examples).
build:
    cargo build --workspace --all-targets

# rustfmt + clippy (deny warnings) + prove the engine compiles without the herdr client.
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo check -p lastcall-engine --no-default-features

# The canonical unit suite: in-module #[cfg(test)] only. Deterministic, no network, no
# sockets except the in-test mock, no git repos except temp fixtures.
test-unit:
    cargo test --workspace --lib --bins

# Integration tier: real git, and the real pinned herdr when LASTCALL_TEST_HERDR_BIN is set.
test-integration:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${LASTCALL_TEST_HERDR_BIN:-}" ]; then
        echo "SKIP herdr-real: LASTCALL_TEST_HERDR_BIN unset (run: just test-integration-herdr)" >&2
    fi
    cargo test --workspace --test 'test_integration_*'

# End-to-end tier (Phase 3/9 replace the placeholder).
test-e2e:
    cargo test --workspace --test 'test_e2e_*'

# All three tiers, in order.
test: test-unit test-integration test-e2e

# The Phase 0 shell scenario harness (docs/spec/01-scenarios.md).
harness:
    bash scripts/harness/scenarios.sh

# ---------------------------------------------------------------------------------------
# herdr: fetch the pinned release, run the isolated real-herdr tests
# ---------------------------------------------------------------------------------------

# Download the pinned herdr release asset for this host into target/herdr/<version>/herdr.
# Skips the download when the file already exists and answers --version correctly.
# Prints the absolute path as the last line of output.
herdr-fetch:
    #!/usr/bin/env bash
    set -euo pipefail
    version="{{herdr_version}}"
    dest="{{herdr_bin}}"
    expected="herdr ${version#v}"
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64)  asset="herdr-macos-aarch64" ;;
        Darwin-x86_64) asset="herdr-macos-x86_64" ;;
        Linux-x86_64)  asset="herdr-linux-x86_64" ;;
        Linux-aarch64|Linux-arm64) asset="herdr-linux-aarch64" ;;
        *) echo "herdr-fetch: unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac
    if [ -x "$dest" ] && [ "$("$dest" --version 2>/dev/null || true)" = "$expected" ]; then
        echo "herdr-fetch: using cached $dest ($expected)" >&2
        echo "$PWD/$dest"
        exit 0
    fi
    mkdir -p "$(dirname "$dest")"
    echo "herdr-fetch: gh release download $version -R herdrdev/herdr -p $asset" >&2
    gh release download "$version" -R herdrdev/herdr -p "$asset" -O "$dest" --clobber
    chmod +x "$dest"
    actual="$("$dest" --version)"
    if [ "$actual" != "$expected" ]; then
        echo "herdr-fetch: expected '$expected', got '$actual'" >&2
        exit 1
    fi
    echo "herdr-fetch: verified $actual" >&2
    echo "$PWD/$dest"

# Fetch the pinned herdr, then run the integration tier against it.
test-integration-herdr:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="$(just herdr-fetch | tail -n 1)"
    LASTCALL_TEST_HERDR_BIN="$bin" just test-integration

# Re-record the herdr fixtures from the real pinned binary (deliverable 6 provenance rule)
# into crates/lastcall-testkit/fixtures/herdr/recorded/ (committed; see the provenance files).
herdr-record:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="$(just herdr-fetch | tail -n 1)"
    dir="$PWD/crates/lastcall-testkit/fixtures/herdr/recorded"
    mkdir -p "$dir"
    LASTCALL_TEST_HERDR_BIN="$bin" LASTCALL_RECORD_DIR="$dir" \
        cargo test -p lastcall-engine --test test_integration_herdr_real -- --nocapture
    ls -la "$dir"
    just fixtures-sync

# Derive the named fixtures from the recordings (each <name>.provenance.md documents the rule).
fixtures-sync:
    #!/usr/bin/env bash
    set -euo pipefail
    f="crates/lastcall-testkit/fixtures/herdr"
    r="$f/recorded"
    cat "$r/snapshot_two_panes.json" > "$f/snapshot_two_panes.json"
    cat "$r/snapshot_two_panes_after_focus.json" > "$f/snapshot_two_panes_after_focus.json"
    cat "$r/subscribe_failure.jsonl" > "$f/subscribe_failure.jsonl"
    # status_working_to_done: the two recorded per-pane lines, then the recorded tab_focused
    # for the agent's tab (the focus that flips done -> idle), so `just probe-hello` replays
    # both transitions through the client's real code paths.
    cat "$r/status_working_to_done.jsonl" > "$f/status_working_to_done.jsonl"
    grep '"tab_focused"' "$r/lifecycle.jsonl" | grep '"tab_id":"w1:t1"' | tail -n 1 >> "$f/status_working_to_done.jsonl"
    echo "fixtures synced from $r:"
    ls -la "$f"

# ---------------------------------------------------------------------------------------
# Repo hygiene
# ---------------------------------------------------------------------------------------

# Enable the committed pre-commit hook (runs `just lint && just test-unit`).
hooks-install:
    chmod +x .githooks/pre-commit
    git config core.hooksPath .githooks
    @echo "hooks installed: core.hooksPath=.githooks"

# ---------------------------------------------------------------------------------------
# Probes and demos (built-artifact passes exercise the release binary)
# ---------------------------------------------------------------------------------------

# Serve a scripted herdr session over a real socket from the mock example.
mock-herdr socket snapshot events *ARGS:
    cargo run -p lastcall-testkit --example mock_herdr -- --socket {{socket}} --snapshot {{snapshot}} --events {{events}} {{ARGS}}

# Release build, then print the effective config from a temp LASTCALL_CONFIG.
probe-config:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    dir="/tmp/lc-probe-$$"
    trap 'rm -rf "$dir"' EXIT
    mkdir -p "$dir/parent"
    cat > "$dir/config.toml" <<EOF
    parent_dirs = ["$dir/parent"]
    draft_dirs = ["_drafts/**"]
    draft_initial = "pending"
    collapse_size_bytes = 1024
    [herdr]
    mode = "auto"
    session = "probe"
    EOF
    echo "--- $dir/config.toml ---"
    cat "$dir/config.toml"
    echo "--- LASTCALL_CONFIG=$dir/config.toml target/release/lastcall config --json ---"
    LASTCALL_CONFIG="$dir/config.toml" ./target/release/lastcall config --json
    echo "--- (launched from $dir, outside parent_dirs) target/release/lastcall config ---"
    (cd "$dir" && LASTCALL_CONFIG="$dir/config.toml" "$OLDPWD/target/release/lastcall" config)

# Release build, then hello-herdr against the mock example scripted with status_working_to_done.
probe-hello:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    cargo build -p lastcall-testkit --example mock_herdr
    dir="/tmp/lc-probe-$$"
    trap 'rm -rf "$dir"' EXIT
    mkdir -p "$dir"
    sock="$dir/herdr.sock"
    fixtures="crates/lastcall-testkit/fixtures/herdr"
    echo "--- mock_herdr --socket $sock ---"
    ./target/debug/examples/mock_herdr --socket "$sock" \
        --snapshot "$fixtures/snapshot_two_panes.json" \
        --snapshot-after "$fixtures/snapshot_two_panes_after_focus.json" \
        --events "$fixtures/status_working_to_done.jsonl" > "$dir/mock.log" 2>&1 &
    mock_pid=$!
    for _ in $(seq 1 200); do [ -S "$sock" ] && break; sleep 0.025; done
    [ -S "$sock" ] || { echo "mock socket never appeared" >&2; cat "$dir/mock.log"; exit 1; }
    echo "--- target/release/lastcall hello-herdr --socket $sock --exit-after 5 ---"
    set +e
    ./target/release/lastcall hello-herdr --socket "$sock" --exit-after 5
    rc=$?
    set -e
    echo "--- hello-herdr exit=$rc ---"
    wait "$mock_pid" || true
    echo "--- mock_herdr output ---"
    cat "$dir/mock.log"
    exit "$rc"

# Run hello-herdr from the dev checkout against the discovered session.
hello-herdr *ARGS:
    cargo run -p lastcall -- hello-herdr {{ARGS}}

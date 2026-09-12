# lastcall task runner. Every CI step and every gate command is a target here.
# New tooling goes into a target rather than an ad-hoc command: the committed permission
# allowlist (.claude/settings.json) covers `just …` and little else.

set shell := ["bash", "-euo", "pipefail", "-c"]

# rustup's proxies must win over any stale system cargo, even in shells whose PATH
# predates the rustup install (agent harness shells snapshot PATH at session start).
export PATH := (if path_exists("/opt/homebrew/opt/rustup/bin") == "true" { "/opt/homebrew/opt/rustup/bin:" } else { "" }) + env("HOME") / ".cargo/bin:" + env("PATH")

# The pinned herdr release used by the real-herdr integration tests (docs/spec/00-spec.md §4.4).
# The only sanctioned network fetch in the repo besides cargo's registry and rustup.
herdr_version := "v0.9.0"
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
    # Safe wrappers only: `nix`, never a direct `libc` call or dependency. A raw
    # `libc::open` would be `unsafe`, and `unsafe_code = "forbid"` is workspace-wide —
    # this grep catches the dependency edge before someone reaches for the escape hatch
    # (docs/spec/96-phase7-kickoff.md, design review F3).
    ! grep -rn --include='*.rs' 'libc::' crates
    ! grep -rn --include='Cargo.toml' '^libc' crates

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

# What the pre-push hook runs (`just hooks-install`): the integration tier, then every
# proptest at 64 cases — the store-backed ones in ops::tests::proptests and the text
# buffer's round trip in tui::textbuf (Phase 8). The unit tier runs them at 8 so every
# commit stays fast. Run it by hand before a push from a machine without the hook. Each
# step says what it is doing; the first failure stops the push.
test-prepush:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "--- test-prepush 1/3: just test-integration ---"
    just test-integration
    echo "--- test-prepush 2/3: PROPTEST_CASES=64 cargo test -p lastcall-engine --lib proptests ---"
    PROPTEST_CASES=64 cargo test -p lastcall-engine --lib proptests
    echo "--- test-prepush 3/3: PROPTEST_CASES=64 cargo test -p lastcall --lib proptests ---"
    PROPTEST_CASES=64 cargo test -p lastcall --lib proptests
    echo "--- test-prepush: green ---"

# All three tiers, in order.
# The scenario suites (docs/spec/01-scenarios.md, one test per ID) against real git.
test-scenarios:
    cargo test -p lastcall-engine --test 'test_integration_scenarios*'

# Rewrite crates/lastcall/tests/golden/status_multi_repo.json from the built binary.
golden-update:
    LASTCALL_UPDATE_GOLDEN=1 cargo test -p lastcall --test test_integration_status_golden

# Rewrite the flag-export goldens (crates/lastcall/tests/golden/flag_export*.md), then
# prove they pass. `flag_export.md` is written by an engine unit test with a FixedClock
# (the binary has no clock override); `flag_export_pty.md` by the TUI's PTY scene.
flag-export-golden:
    LASTCALL_UPDATE_GOLDEN=1 cargo test -p lastcall-engine --lib flags::tests::flags_export_matches_the_golden
    cargo test -p lastcall-engine --lib flags::
    LASTCALL_UPDATE_GOLDEN=1 cargo test -p lastcall --test test_e2e_tui_pty -- pty_flag_note_exports_when_standalone
    cargo test -p lastcall --test test_e2e_tui_pty -- pty_flag_note_exports_when_standalone

# Rewrite the Phase 3 TUI snapshots (crates/lastcall/tests/snapshots/), then prove they pass.
snapshots-update:
    INSTA_UPDATE=always cargo test -p lastcall --test test_e2e_tui_snapshots || true
    cargo test -p lastcall --test test_e2e_tui_snapshots

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

# Regenerate the consumed-surface schema fixture from the **pinned** release (kickoff 11a).
# Never from master and never by hand: the provenance file records the tag and this command.
herdr-schema-fixture:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="$(just herdr-fetch | tail -n 1)"
    out="$PWD/crates/lastcall-testkit/fixtures/herdr/schema/consumed-surface.json"
    cargo run -q -p lastcall-testkit --example herdr_schema_fixture -- "$bin" "$out"
    echo "herdr-schema-fixture: wrote $out"

# Download the **latest** herdr release (not the pinned tag) into target/herdr/<tag>/herdr,
# for the weekly compat check. Prints the absolute path as the last line, like herdr-fetch.
herdr-fetch-latest:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(gh release view -R herdrdev/herdr --json tagName --jq .tagName)"
    if [ -z "$version" ]; then
        echo "herdr-fetch-latest: could not resolve the latest tag" >&2
        exit 1
    fi
    dest="target/herdr/$version/herdr"
    expected="herdr ${version#v}"
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64)  asset="herdr-macos-aarch64" ;;
        Darwin-x86_64) asset="herdr-macos-x86_64" ;;
        Linux-x86_64)  asset="herdr-linux-x86_64" ;;
        Linux-aarch64|Linux-arm64) asset="herdr-linux-aarch64" ;;
        *) echo "herdr-fetch-latest: unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac
    echo "herdr-fetch-latest: latest release is $version" >&2
    if [ ! -x "$dest" ] || [ "$("$dest" --version 2>/dev/null || true)" != "$expected" ]; then
        mkdir -p "$(dirname "$dest")"
        gh release download "$version" -R herdrdev/herdr -p "$asset" -O "$dest" --clobber
        chmod +x "$dest"
    fi
    actual="$("$dest" --version)"
    # The version check is against the tag gh reported, not against herdr_version.
    if [ "$actual" != "$expected" ]; then
        echo "herdr-fetch-latest: expected '$expected', got '$actual'" >&2
        exit 1
    fi
    echo "herdr-fetch-latest: verified $actual" >&2
    echo "$PWD/$dest"

# The real-server subset against the **latest** herdr, for the compat job. A protocol-guard
# refusal, a schema-projection mismatch or any herdr_real_* failure all count as drift.
test-integration-herdr-latest:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="$(just herdr-fetch-latest | tail -n 1)"
    LASTCALL_TEST_HERDR_BIN="$bin" \
        cargo test -p lastcall-engine --test test_integration_herdr_real -- --nocapture --test-threads=1

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

# Enable the committed hooks: pre-commit runs `just lint && just test-unit`, pre-push
# runs `just test-prepush` (integration tier + 64-case proptests).
hooks-install:
    chmod +x .githooks/pre-commit .githooks/pre-push
    git config core.hooksPath .githooks
    @echo "hooks installed: core.hooksPath=.githooks (pre-commit, pre-push)"

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

# Release binary over the golden's three-root fixture: `status`, then `status --json`.
probe-status:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    cargo build -p lastcall-testkit --example fixture_parent
    dir="/tmp/lc-probe-$$"
    trap 'rm -rf "$dir"' EXIT
    mkdir -p "$dir/parent" "$dir/state"
    echo "--- fixture_parent --parent $dir/parent --state-dir $dir/state ---"
    ./target/debug/examples/fixture_parent --parent "$dir/parent" --state-dir "$dir/state"
    export LASTCALL_CONFIG="$dir/state/config.toml" LASTCALL_STATE_DIR="$dir/state"
    export HOME="$dir/state/home" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
    echo "--- (cd $dir/parent) lastcall status ---"
    (cd "$dir/parent" && "$OLDPWD/target/release/lastcall" status)
    echo "--- (cd $dir/parent) lastcall status --json ---"
    (cd "$dir/parent" && "$OLDPWD/target/release/lastcall" status --json)

# Release binary `watch --exit-after 8` over the same fixture while the example commits in
# repo A at t+2 s and edits there at t+4 s: the B1 notice must appear. `--poll 2` is the
# backstop for hosts whose filesystem events are late or missing (a wedged fseventsd): the
# notice then arrives through the HEAD poll instead of the watch.
probe-watch:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    cargo build -p lastcall-testkit --example fixture_parent
    dir="/tmp/lc-probe-$$"
    trap 'rm -rf "$dir"' EXIT
    mkdir -p "$dir/parent" "$dir/state"
    ./target/debug/examples/fixture_parent --parent "$dir/parent" --state-dir "$dir/state"
    export LASTCALL_CONFIG="$dir/state/config.toml" LASTCALL_STATE_DIR="$dir/state"
    export HOME="$dir/state/home" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
    ./target/debug/examples/fixture_parent --parent "$dir/parent" --state-dir "$dir/state" --late-ops > "$dir/late.log" 2>&1 &
    late_pid=$!
    echo "--- (cd $dir/parent) lastcall watch --exit-after 8 --poll 2 ---"
    (cd "$dir/parent" && "$OLDPWD/target/release/lastcall" watch --exit-after 8 --poll 2)
    wait "$late_pid"
    echo "--- late-ops ---"
    cat "$dir/late.log"

# The sponsor's interactive look: the release binary's `tui --poll 1` over the fixture
# parent, with a temp state dir it creates (never `~/.local/state/lastcall`; HOME and the
# git config locations are pointed away exactly as `probe-watch` does). The two env lines
# are printed first so the same screen can be re-run by hand; edit a file under the printed
# parent from another shell and watch the counts change. `q` quits. The fixture is left in
# place for that re-run — remove it with the `rm -rf` printed at the end.
probe-tui:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    cargo build -p lastcall-testkit --example fixture_parent
    dir="/tmp/lc-probe-$$"
    mkdir -p "$dir/parent" "$dir/state"
    echo "--- fixture_parent --parent $dir/parent --state-dir $dir/state ---"
    ./target/debug/examples/fixture_parent --parent "$dir/parent" --state-dir "$dir/state"
    export LASTCALL_CONFIG="$dir/state/config.toml" LASTCALL_STATE_DIR="$dir/state"
    export HOME="$dir/state/home" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
    echo "export LASTCALL_CONFIG=$LASTCALL_CONFIG"
    echo "export LASTCALL_STATE_DIR=$LASTCALL_STATE_DIR"
    echo "--- (cd $dir/parent) lastcall tui --poll 1   [q quits; edit under $dir/parent from another shell] ---"
    echo "--- accept keys work: a = hunk (or file/group/repo), A = file, ctrl-a = everything (y confirms above 10 files);"
    echo "--- a relaunch with the same two exports shows what is still pending ---"
    (cd "$dir/parent" && "$OLDPWD/target/release/lastcall" tui --poll 1)
    echo "--- fixture left at $dir (rm -rf $dir when done) ---"

# The transcript form of the live-update demo, for a human without a second terminal: the
# PTY harness (crates/lastcall-testkit/src/pty_tui.rs) drives the release binary's
# `tui --poll 1` over a fresh fixture parent in a 100×30 pseudo-terminal, appends a line to
# alpha/f1, waits for the row's counts to change on screen, opens the diff, prints the vt100
# screen as text and the exit code after `q`. About three seconds; nothing is left behind.
probe-tui-screen:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    LASTCALL_PROBE_BIN="$PWD/target/release/lastcall" \
        cargo test -p lastcall --test test_e2e_tui_pty probe_tui_screen -- --ignored --nocapture

# `probe-tui` with the scans stretched: scripts/slowgit/git goes first on PATH and sleeps
# before the scan-only git calls (SLOWGIT_MS per call, default 800; four per root), with
# one root slower (SLOWGIT_SLOW_REPO, default alpha; SLOWGIT_SLOW_MS per call, default
# 2500) — the launch hold's counter and per-root ✓ marks, as a user with hundreds of repos
# or a slow disk would see them (docs/dev/tui.md "Seeing the hold slowly"). Discovery,
# before the screen opens, runs at full speed. `r` (refresh) is stretched the same way.
probe-tui-slow:
    #!/usr/bin/env bash
    set -euo pipefail
    export PATH="$PWD/scripts/slowgit:$PATH"
    export SLOWGIT_SLOW_REPO="${SLOWGIT_SLOW_REPO:-alpha}"
    echo "--- slow git on PATH: SLOWGIT_MS=${SLOWGIT_MS:-800} per scan call, ${SLOWGIT_SLOW_REPO} at SLOWGIT_SLOW_MS=${SLOWGIT_SLOW_MS:-2500} ---"
    just probe-tui

# The performance baseline (docs/dev/bench.md; not a gate): the four scenarios of
# crates/lastcall/tests/test_bench.rs — 100 clones / 4,000 rows, one 100,000-line diff, a
# 1,000-file burst under watch, a 50,000-file drop against the row cap — on the RELEASE
# build only, one `BENCH <scenario> <metric>=<value>` stderr line per metric. Fixtures are
# built outside the timed regions under temp dirs the test removes; about four minutes.
bench:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p lastcall
    cargo test --release -p lastcall --test test_bench -- --ignored --nocapture --test-threads=1
    echo "--- bench done: paste the BENCH lines above into docs/dev/bench.md with the machine block, date and commit ---" >&2

# cargo-deny against deny.toml: advisories, licences, bans (install: cargo install cargo-deny).
audit:
    cargo deny check advisories licenses bans

# The fresh-container install smoke: `ubuntu:24.04` with nothing on it installs a published
# asset, verifies its checksum, runs it, and takes an update from a release served inside the
# container (scripts/install-smoke.sh has the detail). Both platforms by default, because
# without the amd64 leg on an Apple-silicon host x86_64 Linux would ship untested. Exits 2
# when Docker is not there or not running.
#   just install-smoke                      # the latest release, updating to the next patch
#   just install-smoke v0.1.0 v0.1.1        # two real releases
#   just install-smoke --from-dir ./dist    # a rehearsal's artifacts, no release needed
#   just install-smoke --self-test          # next_version's cases only: no docker, no network
install-smoke *ARGS:
    scripts/install-smoke.sh {{ARGS}}

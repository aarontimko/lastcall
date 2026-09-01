# lastcall task runner. Every CI step and every gate command is a target here.
# Phase 1 (docs/spec/90-phase1-kickoff.md, deliverable 2) fills in the rest.

set shell := ["bash", "-euo", "pipefail", "-c"]

# rustup's proxies must win over any stale system cargo, even in shells whose PATH
# predates the rustup install (agent harness shells snapshot PATH at session start).
export PATH := (if path_exists("/opt/homebrew/opt/rustup/bin") == "true" { "/opt/homebrew/opt/rustup/bin:" } else { "" }) + env("HOME") / ".cargo/bin:" + env("PATH")

herdr_version := "v0.8.2"

# Run any cargo command with the pinned toolchain on PATH: `just cargo add serde`
cargo *ARGS:
    cargo {{ARGS}}

# Print the toolchain the targets will use.
toolchain:
    which cargo && cargo --version && rustc --version

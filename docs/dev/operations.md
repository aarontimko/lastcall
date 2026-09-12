# Operating lastcall after v0.1.0

The standing handoff, written on 2026-09-12 when the last gate of the build program closed.
It is for whoever cuts the next release or answers the next red job, including the
maintainer six months from now. The design record is `docs/spec/00-spec.md`; this page is
only what has to keep happening and what must not drift.

## What shipped

`v0.1.0` is on the Releases page: four binaries (macOS and Linux, arm64 and x86_64), a
`SHA256SUMS` file, one build attestation over those five files, and the `CHANGELOG.md`
section as the notes. `lastcall update` moves an installed binary to the newest release and was proven
live from `v0.1.0-rc.1` to `v0.1.0`. The repository is public, `main` is protected, CI and
the scans run on every pull request, and Dependabot files version bumps weekly.

## Cutting a release

Releases are cut by hand for the whole v0.1 line (ruling P14 in
`docs/spec/99-phase9b-release-kickoff.md`). The steps, in order:

1. A pull request sets `[workspace.package].version` in `Cargo.toml`, runs
   `cargo update --workspace` so the lockfile follows, and adds the version's section to
   `CHANGELOG.md` with its date in the heading (`## 0.2.0 - YYYY-MM-DD`). The release job
   takes the notes from the first heading that reads `## <version>` followed by a space or
   nothing (an rc uses the section of the version it is a candidate for) and fails, after
   the four builds, when no such section exists.
2. The maintainer merges it, pulls `main`, and tags the merge commit with an annotated tag
   named `v` plus the crate version, then pushes that tag. The pre-push hook runs the
   integration tier first; let it finish.
3. The tag starts `.github/workflows/release.yml`. Its `verify` job refuses a tag that is
   not exactly `v<crate version>`, so a tag on the wrong commit fails before anything is
   built. A tag containing a hyphen (`v0.2.0-rc.1`) publishes a prerelease.
4. When the run is green: download all five files, `shasum -a 256 -c SHA256SUMS`,
   `gh attestation verify <asset> --repo aarontimko/lastcall` for each binary, and
   `just install-smoke <previous tag> <new tag>`, which installs the previous release in a
   fresh Ubuntu container on both Linux architectures and takes the update to the new one.
5. Nothing else. There is no Homebrew tap (post-v1), and publishing to crates.io was never
   decided either way.

A rehearsal without a tag is `gh workflow run release.yml --ref <branch>` (add
`-f targets=all` for the four legs); it uploads the assets as workflow artifacts and creates
no release. `gh run download <run id> -D dist` fetches them, and
`just install-smoke --from-dir ./dist` runs the container smoke against them.

## The jobs that run on their own

| Job | When | Green means | When red |
|---|---|---|---|
| `ci.yml` | every pull request, every push to `main` | lint, unit, integration and e2e on Ubuntu and macOS; the macOS integration leg is advisory | a real failure or a flaky runner; re-run with `gh run rerun <id>` before reading further. The five required checks on `main` are check contexts, three jobs times two runners minus macOS integration; renaming a job or a runner means updating the branch protection in the same change (`docs/dev/publishing.md` step 3). |
| `scans.yml` | every pull request, Mondays 06:00 UTC | `cargo-deny` (advisories, licences, bans against `deny.toml`), CodeQL, and on pull requests the dependency review | an advisory: bump the crate, or add a dated `ignore` entry in `deny.toml` with the reason; a licence: `deny.toml`'s allow list, with the reason. CodeQL findings appear under Security, Code scanning. |
| `herdr-compat.yml` | Mondays 06:00 UTC, and pull requests that touch its inputs | the real-herdr integration subset and the consumed-surface schema check pass against herdr's latest release | it files one issue labelled `herdr-compat`. That is early warning, not a break: `ci.yml` uses the pinned release and stays green. To adopt the new herdr: bump `herdr_version` in the `justfile`, run `just herdr-schema-fixture`, read the fixture diff and update `consumed-surface.json.provenance.md` beside it, run `just test-integration-herdr`, and if the protocol number moved widen `SUPPORTED_PROTOCOLS` with an amendment to `docs/spec/00-spec.md` §5.2. |
| Dependabot | Mondays 06:00 UTC | grouped minor and patch bumps for cargo and for Actions, up to five open each | nothing to do beyond merging when CI is green. A major bump of an action in `release.yml` must keep the SHA pin with its version comment. |

Secret scanning with push protection, private vulnerability reporting and Dependabot alerts
are on. A vulnerability report arrives under Security, Advisories; `SECURITY.md` is the
public contract for it. Issues arrive with `needs-triage` from the two forms; questions and
ideas go to Discussions (Q&A, Ideas).

## What must not drift

- **`docs/spec/00-spec.md` §5 and §6 are frozen.** A change to the herdr surface consumed or
  to our own contracts is an amendment proposed in the PR that needs it and ratified by the
  merge; the Amendments table at the end of the spec is the record. Gates change only by
  editing §8 (§3.4).
- **Tests never touch the real herdr configuration or socket.** Everything goes through the
  injected `Env`; `docs/dev/testing.md` has the isolation rules and the grep that enforces
  `std::env` living in one file.
- **Nothing is ever written inside a repository lastcall watches.** The ledger lives under
  `$LASTCALL_STATE_DIR`, else `$XDG_STATE_HOME/lastcall`, else `~/.local/state/lastcall`.
- **The crate version equals the tag** (`v0.1.0` for `0.1.0`); an rc is its own crate
  version (`0.1.0-rc.1`). The release workflow enforces it.
- **Every action in `release.yml` is pinned by commit SHA** with its version in a comment,
  because that workflow signs bytes other people run. `ci.yml`, `scans.yml` and
  `herdr-compat.yml` pin by tag.
- **`unsafe_code = "forbid"` workspace-wide and no direct `libc`**; the pre-commit hook greps
  for the dependency edge.
- **The public surface keeps its house style**: README, `CHANGELOG.md`, the four user docs
  under `docs/`, the workflows and the scripts carry no em-dashes, no email addresses, no
  home paths and none of the build program's process vocabulary.
- **The maintainer pushes and tags; agents do not.** Every branch crosses the network by a
  human hand, and `main` takes merge commits through a pull request with the five checks
  green, admins included.

## What is kept, what can go

- **Keep:** the maintainer's own state directory (his review ledger) and `~/.config/lastcall`
  (his `config.toml`); the private notes directory outside the repository that holds the review
  records, the disclosure denylist and the session handoff (`z_ignore/`, gitignored).
- **Removable, regenerated on demand:** `target/` including `target/herdr/<version>/`
  (`just herdr-fetch` re-downloads the pinned release), the `just probe-*` fixtures under
  the system temp directory, the demo staging directory for the README GIF
  (`docs/demo/stage.sh` rebuilds it).
- **No stacks, containers or volumes outlive a run.** The install smoke starts and removes
  its own containers.

## Deferred, not designed

Each of these is recorded with its reason and its trigger; none is a promise.

- **The herdr scope circle-back** (`docs/spec/00-spec.md` §3.5): whether the containment
  fallback that guesses a workspace's repositories from every pane's directory earns its
  complexity, or only the explicit workspace signal should scope the list; with it the scope
  notice's cost on the hint line and the status-line age. Decided together, when the
  maintainer is fresh.
- **release-please** for cutting releases (P14): three exit conditions before it replaces
  hand tags, all written in the ruling.
- **Post-v1 by design** (§8): the `agent.prompt` flag-and-discuss loop, sidebar pending
  counts in herdr, herdr plugin packaging, hook-based attribution adapters, per-branch seen
  trees, rename override carry, a Homebrew tap, Windows. `ExportContext.attribution` is
  ledgered in §11 (kickoff ruling P8).
- **The deferral ledger** (`docs/spec/00-spec.md` §11): every accepted risk with its
  hardening shape and trigger, reviewed in full before the release (§10 2026-09-10).

## Where the evidence is

| What | Where |
|---|---|
| The design record, the gate checklists with their evidence, the decision log | `docs/spec/00-spec.md` §8, §10 |
| Each phase's operational spec and its review fold | `docs/spec/9N-phaseN-kickoff.md` |
| The scenario plan the integration tests are named after | `docs/spec/01-scenarios.md` |
| The performance baseline and every bench run | `docs/dev/bench.md` |
| The release runs | `v0.1.0-rc.1`: Actions run 34712558530; `v0.1.0`: run 34714870423 |
| The day the repository went public | `docs/dev/publishing.md`; §10 2026-09-12 |
| The adversarial review records and the maintainer's install and update transcripts | the private notes directory, not committed (§10 cites them by file name) |
| How the build program was run | `docs/spec/00-spec.md` §3, and the README's last sentence under Status |

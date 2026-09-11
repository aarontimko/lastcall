# Security policy

## Supported versions

| version | supported |
|---|---|
| the latest minor release | yes, security fixes are published here |
| any earlier release | no, upgrade to the latest minor |
| pre-release builds from `main` | no |

lastcall has not reached v0.1.0 yet. Until it does, `main` is the only thing there is, and
it carries no support promise.

## Reporting a vulnerability

Use GitHub private vulnerability reporting on this repository: the **Security** tab, then
the **Report a vulnerability** button. That opens a private advisory only the maintainer
can see.

Do not open a public issue, a discussion, or a pull request for a security problem. A
public report is a disclosure.

If private reporting is unavailable to you, send a direct message to
[@aarontimko](https://github.com/aarontimko) on GitHub asking for a private channel, and
say nothing about the issue itself in that message.

## What to include

- What the problem is, in one or two sentences.
- The version: the output of `lastcall --version`, or the commit SHA if you built from
  source.
- Your OS and terminal.
- Steps to reproduce, or a proof of concept.
- What an attacker gets out of it, and what they need first (local shell? a repository you
  can write to? a herdr socket?).
- Anything you already know about a fix.

## Response

- Acknowledgement within 7 days.
- For a confirmed report, a fix or a decision not to fix within 30 days of the
  acknowledgement.
- You are credited in the advisory unless you ask not to be.

## Threat model

What lastcall touches, so you can judge whether something is in scope:

- **Reads** git repositories under the parent directories named in `config.toml`, the
  launch directory when it lies outside them (or when there is no config), and any
  `draft_dirs` (plain directories reviewed as if they were repositories). It runs git
  through a read-only allowlist of subcommands.
- **Writes** its own state directory, `~/.local/state/lastcall` by default and
  `LASTCALL_STATE_DIR` when set. Nothing is written to the repositories it watches during
  a scan.
- **Writes the working tree** only on an explicit action you take: accepting does not touch
  files, but restoring a hunk or a file, and saving from the built-in editor, do. Each of
  those is a compare-and-swap against what was on screen and is refused if the file
  changed underneath. Your external editor writes the file itself; lastcall re-reads it
  when the editor exits and asks before treating the result as reviewed.
- **Talks to a local herdr socket** when one is present: a Unix domain socket on the same
  machine, found through the `HERDR_*` environment a herdr pane sets or herdr's own
  config directory. The herdr surface is treated as
  untrusted input (unknown fields are ignored, never rejected). With no socket, lastcall
  runs standalone.
- **Reaches the network** in two places once it is released, both aimed at
  `github.com` and nowhere else. `lastcall update` downloads a release asset over HTTPS
  and verifies a SHA-256 checksum before it replaces its own binary. The TUI also asks the
  GitHub releases API once a day, in the background after the first frame, whether a newer
  version exists; that request carries the lastcall version as its user agent and nothing
  else, and `check = false` under `[update]` in `config.toml` turns it off. Nothing else in
  the shipped binary makes a network call. (`just herdr-fetch` downloads a pinned herdr
  release, but that is a developer and CI command, not something the binary does.)

Out of scope: anything that needs an attacker to already have arbitrary code execution as
your user, and the security of the repositories or the agents lastcall is watching.

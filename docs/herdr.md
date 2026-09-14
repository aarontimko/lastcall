# lastcall with herdr

[herdr](https://github.com/herdrdev/herdr) runs coding agents in terminal panes and knows
which one is working, which one is blocked and which one has just finished. lastcall knows
what changed on disk. Run lastcall inside a herdr pane and the two are one screen: the
repository list tells you which agent needs you, and a note you write on a hunk goes
straight to the agent that wrote it.

None of this is required. With no herdr session lastcall runs standalone, says so in the
header, and everything except the parts on this page works exactly the same.

## What the overlay adds

**A dot on each repository row**, before the name, with the agent count when there is more
than one (`⚑3 alpha`):

| dot | means |
|---|---|
| `⚑` bold | an agent finished in a tab you were not watching. Dim once you have acknowledged it. |
| red `●` | an agent is blocked and waiting on you |
| yellow `●` | an agent is working |
| dim `·` | herdr reported a status word this version does not know |
| nothing | idle, or finished and already dealt with |

A repository with several agents shows the most urgent of them, in the order blocked,
finished, working, idle, unknown. A repository with **nothing pending** is still listed
while it has a finished or blocked agent, so there is somewhere to press `enter`, and it
reads `nothing pending · agent done` in both panes. A working or idle agent only annotates a
repository that is listed for its own reasons.

Every dot disappears while the link is down. A lastcall that is reconnecting, or running
standalone, claims nothing about your agents rather than showing you yesterday's answer.

The finished flag is one alert per episode. It opens the first time an agent goes finished
and closes when that stops being true, so a repository that finishes, is dealt with, and
finishes again alerts you twice, not continuously. Acknowledging is local to the lastcall
you are sitting in front of: a second one over the same session tracks its own.

**Three extra keys**, rebindable like any other (see [`config.md`](config.md)):

| key | action | what it does |
|---|---|---|
| `d` | `ack` | acknowledge the finished flag on the selected repository. The `⚑` dims and the repository drops out of any pending notification. It does nothing on a blocked repository: there is no episode to acknowledge, and the hint is not offered there either. |
| `g` | `jump` | focus that agent's pane in herdr. Works on a finished or a blocked repository, since both have a pane to go to. |
| `w` | `scope` | show every watched repository, or only the ones in this workspace. |

Clicking the dot selects that repository and acknowledges it in one gesture.

**A badge in the header** saying what the link is doing: `herdr <version>` when connected,
`herdr ⟳` while reconnecting, `standalone` when there is no link, and, under `mode = "on"`,
`standalone: <reason>` when one was asked for and did not happen. Click the badge to put the
whole sentence in the status line, which is how you read a reason too long for the header.

**A flag that lands in the agent's input box.** When you write a note with `m`, lastcall
looks at which agents are working in that repository:

- exactly one, and the note is staged into that agent's pane;
- several, and lastcall asks which one rather than guessing. The picker is live, so a pane
  that appears or disappears while it is open changes the list;
- none, or no herdr at all, and the note is written to a file under the state directory
  instead.

Staged means pasted, not submitted. The note sits in the agent's input box with your cursor
after it, and you press Enter yourself; the status line says `flagged <file> · staged to
<agent>`. The flag is written to the ledger **before** any of this, so cancelling the picker
with `esc` loses nothing: the file keeps its `⚑`, the note still reads beside the hunk it is
about, and the status line says `flagged <file> · not sent`. A send that the pane refuses is
the same story, and says so.

## How the session is found

In order, stopping at the first that works:

1. `HERDR_SOCKET_PATH`, which herdr sets in every pane it owns. If you are running inside a
   herdr pane, this is the answer and nothing else is tried.
2. A pinned session name: `session` under `[herdr]` in the config file, or the
   `HERDR_SESSION` environment variable when the config has no pin. That names
   `<herdr config dir>/sessions/<name>/herdr.sock`.
3. The default socket, `<herdr config dir>/herdr.sock`, if it answers.
4. Every socket under `<herdr config dir>/sessions/*/herdr.sock` that answers. Exactly one
   live session is used. Several, with no pin, and lastcall connects to none of them and
   says so: guessing which of your sessions you meant is not a thing it will do.

The herdr config directory is `$XDG_CONFIG_HOME/herdr`, else `~/.config/herdr`.

`mode` under `[herdr]` decides how hard it looks: `auto` (the default) links when there is
something to link to and runs standalone otherwise, `on` does the same but puts the reason
for a failed link in the header where you can see it, and `off` never looks at all.

**A herdr that speaks a protocol this version does not know is a notice, not a failure.**
lastcall says so in one line, runs standalone, and carries on. The reverse is also true: a
herdr newer than this lastcall may add fields that lastcall ignores rather than choking on.

### The environment herdr sets

These come from herdr itself, in the pane it starts lastcall in. You do not set them by
hand.

| variable | what lastcall does with it |
|---|---|
| `HERDR_SOCKET_PATH` | the session to talk to |
| `HERDR_SESSION` | a session name, used only when the config file has no pin |
| `HERDR_WORKSPACE_ID` | which workspace this pane belongs to, which is what the scope below is derived from |
| `HERDR_PANE_ID` | which pane is ours, so that our own activity is never mistaken for yours |

That last one is worth a sentence. herdr reports each pane's foreground working directory,
and in the pane lastcall runs in the foreground process is lastcall and the `git` commands
it spawns. Without knowing which pane is its own, lastcall would see itself moving through
your repositories during a scan and conclude that the workspace had changed. It knows, and
its own pane never counts as evidence.

## Workspace scope

When the pane's workspace can be identified, lastcall narrows the list to the repositories
that workspace is actually working in, and says so on the bottom line:

```text
scope: <workspace> · 3 repos hidden (w shows all)
```

`w` turns it off and on for the session. `scope = "all"` under `[herdr]` starts it off, and
the welcome on your first launch inside a herdr session offers to write that line for you. A
hidden repository is still watched and still scanned: the scope is a view, not a filter, so
turning it off shows a current list rather than starting a fresh scan.

Nothing is listed until the first scope verdict arrives, including the verdict "no scope".
The wait is part of the launch screen rather than a second one. This exists because the
alternative is worse: a repository listed for one frame and then hidden looks like a bug,
and a single repository shown while others are still being checked reads as "this is the
only one with changes", which may be false.

## Notifications

`toast = true` under `[herdr]`, which is the default, asks herdr to show a desktop
notification when a repository first goes ready.

**herdr has to agree.** Its own setting is off by default, so in `~/.config/herdr/config.toml`:

```toml
[ui.toast]
delivery = "herdr"
```

Without it, every notification lastcall asks for is declined and nothing is retried.

The notification is deliberately not instant. herdr shows its own toast when an agent
finishes and declines anything else while one is on screen, so lastcall waits about seven
seconds and then sends one notification naming every repository that went ready in that
window and is still unacknowledged: `lastcall: alpha ready for review`, or
`lastcall: 3 repos ready for review` with the names in the body. If herdr is busy it is
asked once more, five seconds later, and never a third time.

## Seeing it work

herdr reports an agent as finished **only when the tab you were looking at was a different
one**. Focusing the tab an agent is running in flips it back to idle silently. So a
side-by-side pane in the same tab never raises a flag: you watched it happen. To see the
whole thing:

1. In herdr, open a workspace on the **parent directory** of your repositories and run
   lastcall in a pane there. The badge should read `herdr <version>`.
2. In a **different** herdr workspace, in its own tab, open one of those repositories and
   start an agent in it.
3. Switch back to the lastcall tab and leave it focused. That repository's row shows a
   yellow `●` while the agent works.
4. When the agent finishes, the row flips to a bold `⚑`, the repository is listed even if
   nothing is pending, and a notification follows a few seconds later if herdr is set up for
   it.
5. `g` jumps to the agent's pane. `d` dims the flag without leaving the review.

If the flag never appears, check which tab you were watching.

## Not yet: the sidebar count

herdr can show a token per workspace in its sidebar, and the obvious one for lastcall to
publish is the number of files waiting to be reviewed, so that a workspace you are not
looking at can say `3 pending` without opening anything. That is planned rather than
shipped: it needs lastcall to report a count to herdr on every rescan, and it is on the
list for after 0.1.0. Nothing about the current integration depends on it.

## Related

- [`config.md`](config.md) for the `[herdr]` table and the environment variables.
- [`review-loop.md`](review-loop.md) for what to do once a repository asks for your
  attention.
- [`dev/tui.md`](dev/tui.md) for the implementation, including the rollup rules and the
  staging format.

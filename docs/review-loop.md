# The review loop

An agent has been working. You come back to a directory of repositories and want to know
what changed, decide file by file what stays, and tell the agent about the parts that do
not. That is the loop, and this page walks it once: launch, read, accept, restore, flag,
edit, copy.

Every screen below comes from the same three repository fixture the test suite draws, at
100 columns.

## Running along with it

```sh
just probe-tui
```

builds the release binary, creates that fixture under a fresh temporary directory, points
a real lastcall at it and drops you in. Nothing outside the temporary directory is touched
and no repository of yours is involved, so every key on this page is safe to try. The
recipe prints the directory it used, and `q` quits. A second run starts over.

The fixture has three roots:

- **alpha**, a git repository with three modified files, one of them a real Rust source
  file with two separate hunks;
- **beta**, a git repository with two added files, one of them pulled in by a fetch rather
  than written locally, and a second branch with something on it;
- **notes**, a plain directory that is not a git repository at all, reviewed as a root of
  its own.

## 1. Launch

lastcall lists nothing until every repository has reported. During the wait the header says
the one count it knows, `lastcall  3 repos · checking status…`, and the pane names the
repositories it found:

```text
discovered 3 repos, checking status…

✓ alpha  main
  beta  main
  W/notes  draft

1 of 3 checked · 4 files pending so far · 3s
```

The first second has no digits in it, so a fast launch is one calm frame rather than a
flash of numbers. After that the counter appears, and each repository that has answered
gets a tick in the leading column. The ticks line up, so the repository holding everything
up is the gap in the column rather than something you have to read for.

Why wait at all: a list that shows one repository and then three looks, for that moment,
exactly like "this is the only one with anything in it", which may be the opposite of the
truth. One screen, once.

Then the hold ends and everything is on screen at once:

```text
lastcall  3 repos · 6 files · 8 hunks  standalone  [Accept All]                           watching W
┌──────────────────────────┬───────────────────────────────────────────────────────────────────────┐
│alpha                     │f1  M  +1 −1                           [A accept file] [U restore file]│
│  main · 3 files          │@@ -1,4 +1,4 @@                         [a accept] [u restore] [m flag]│
│  M f1  +1 −1             │-a1                                                                    │
│  M f2  +1 −0             │+A1                                                                    │
│  M parse.rs  +10 −2      │ a2                                                                    │
│──────────────────────────│ a3                                                                    │
│beta                      │ a4                                                                    │
│  main · 2 files          │                                                                       │
│  A u1  +1 −0  [upstream] │                                                                       │
│  A u2  +2 −0  [mixed]    │                                                                       │
│  upstream · 1 file       │                                                                       │
│──────────────────────────│                                                                       │
│W/notes                   │                                                                       │
│  draft · 1 file          │                                                                       │
│  M n2.md  +2 −0          │                                                                       │
└──────────────────────────┴───────────────────────────────────────────────────────────────────────┘
↑↓ select  ⏎ open  n/p hunk  a accept hunk  A accept file  t hide empty  ? help  q quit
```

Left is what is waiting, grouped by repository and then by branch. Right is the selected
file. `standalone` in the header means no herdr session was found: see
[`herdr.md`](herdr.md) for what appears there instead when there is one.

Two labels are worth knowing on sight. **`[upstream]`** means every line in that file
arrived from a fetch, so it is somebody else's work and not the agent's. **`[mixed]`**
means some of it did and some of it did not. They are a hint about who to ask, not a
restriction: the keys all work the same.

A watched folder that is not a repository carries the folder above it in its name, so the
`W/notes` row says which `notes` it is when more than one is being watched. Selecting that
row puts the folder's own location on a dim second line under the name, with your home
directory written as `~`. How many parent folders a name carries is `draft_dir_parents` in
[`config.md`](config.md); a repository keeps its own directory name either way.

What such a folder covers is the entry that matched it. `notes` is the files in `notes/`
and nothing below it, and `notes/**` is the folder and everything below it. Work outside
that is left alone and never listed, so a deep scratch folder cannot fill the pane with
rows nobody asked to review.

`↑` and `↓` move one entry at a time. `Home` and `End` (Fn-Left and Fn-Right on a Mac laptop
keyboard) jump to the first and the last entry of the whole list, and `{` and `}` jump to the previous and the next repository's own row,
which is quicker than walking through a long pile a line at a time. Option with an arrow key
does what the braces do, in terminals that send it (see [`config.md`](config.md)).

`t` hides repositories with nothing pending, `f` shows full paths instead of basenames,
`o` shows `org/repo` instead of the directory name, `c` wraps or clips long diff lines,
`?` lists every key, live, including anything you have rebound.

`s` sets one repository aside: it asks for a number of days, one by default and 365 at the
most, and then drops it out of the list until that many days have gone by. It is a view and
nothing more. lastcall keeps watching and scanning the repository the whole time,
`lastcall status` keeps reporting it, and it comes back into the list on its own when an
agent raises a flag on it. `S` lists the ones you have set aside alongside everything else,
each saying the date it comes back; `s` on one of those wakes it now. The bottom line keeps
count either way.

## 2. Read

`enter` opens the selected file, `n` and `p` walk its hunks, `tab` moves focus between the
panes and `esc` goes back to the list. A file with more than one hunk shows them all, each
with its own controls:

```text
lastcall  3 repos · 4 files · 6 hunks  standalone  [Accept All]                           watching W
┌──────────────────────────┬───────────────────────────────────────────────────────────────────────┐
│alpha                     │f1  M  +3 −3                           [A accept file] [U restore file]│
│  main · 1 file           │@@ -42,7 +42,7 @@                       [a accept] [u restore] [m flag]│
│  M f1  +3 −3             │ line 42                                                               │
│──────────────────────────│ line 43                                                               │
│beta                      │ line 44                                                               │
│  main · 2 files          │-line 45                                                               │
│  A u1  +1 −0  [upstream] │+LINE 45 (edited)                                                      │
│  A u2  +2 −0  [mixed]    │ line 46                                                               │
│  upstream · 1 file       │ line 47                                                               │
│──────────────────────────│ line 48                                                               │
│W/notes                   │                                                                       │
│  draft · 1 file          │@@ -75,6 +75,6 @@                       [a accept] [u restore] [m flag]│
│  M n2.md  +2 −0          │ line 75                                                               │
│                          │ line 76                                                               │
│                          │ line 77                                                               │
│                          │-line 78                                                               │
│                          │+LINE 78 (edited)                                                      │
│                          │ line 79                                                               │
│                          │ line 80                                                               │
└──────────────────────────┴───────────────────────────────────────────────────────────────────────┘
↑↓ scroll  ← back  n/p hunk  a accept hunk  A accept file  t hide empty  ? help  q quit
```

Every bracketed control on the right is also a click target, and every one of them has a
key. Use whichever you prefer.

A line too long for the pane is wrapped onto as many rows as it needs, broken at a space
where there is one, so the end of a long line is on the screen with the rest of it. `c`
(or Option-z) turns that off and back on for the session, and `[ui] wrap` in
[`config.md`](config.md) decides which way it opens. A single line will not take the
whole pane: past a few rows short of it the line stops and the last row ends in a dim
count of what is not shown, so the lines under it stay reachable. Copying is unaffected
either way, because `y` copies lines and not rows.

A lockfile, a file over 512 KiB, or anything with a NUL byte early in it appears as a
single collapsed row saying how big it is rather than a screenful of noise. `e` expands it
into real hunks if you actually want to look. Which files collapse is configurable:
[`config.md`](config.md).

Inside a watched folder the size limit decides what is read rather than what is listed. A
file at or over the limit is never opened, so nothing large is hashed to produce a diff. One
the folder has recorded before keeps its row when it changes and reads `not read (over
512 KiB)`, with no diff and no counts; `a` accepts it like any other row, and `z` puts it
back. A file the folder has never recorded keeps its row the same way once it carries a
flag, so flagging something is enough to keep it in front of you however large it grows.
The rest are counted in a single line, `3 files over 512 KiB not read`, instead of a
row each.

## 3. Accept

Accepting means "I have seen this and it is fine". Nothing on disk changes. What changes
is that lastcall stops showing it to you, and the next time it scans, only what happened
after this point is pending.

- `a` accepts the hunk under the cursor, and nothing larger. On a file with no hunks to
  point at (binary, collapsed, deleted, unreadable) it accepts that file.
- `A` accepts the whole entry: the whole file whatever hunk you are on, and in the list the
  whole branch group or the whole repository from its row. A repository of ten files or
  fewer goes at once, so `A` is the key that takes a lot in one keystroke and `a` is the
  one you can lean on.
- `ctrl-a` accepts everything listed across every repository. Above ten files it asks
  first, naming the count.

The cursor then moves the way you would want it to: accept the last hunk in a file and it
goes on to the next file, accept the last file in a repository and it lands on the
repository row rather than jumping somewhere else.

You do not have to finish. Quit halfway through and what you accepted stays accepted; the
rest is still there next time.

`z` undoes the last accept in the selected repository, and again for the one before it, up
to the last twenty. Like an accept it changes nothing on disk: the files it covers simply go
back to pending, with any flags you put on them still there, and the cursor moves to the
first of them. Each repository has its own stack, and the stack outlives the session, so a
morning's accept can be undone that evening. Saving a file in the built-in editor is on the
stack too, because saving marks the file reviewed (see [Edit](#6-edit)); undoing that one
puts your own edit back on the list as something to look at, which is the point. `z` with
nothing left to undo says so and does nothing.

## 4. Restore

Restoring is the opposite, and it does touch the disk: `u` puts the hunk under the cursor
back the way it was, `U` puts the whole file back. Because it is the one destructive key
in the program, the file form asks:

```text
│  upstream · 1 file       │    ┌ restore ────────────────────────┐                                │
│──────────────────────────│    │ Restore f1 · 1 hunk?            │                                │
│W/notes                   │    │                                 │                                │
│  draft · 1 file          │    │ y / ⏎ confirm    n / Esc cancel │                                │
│  M n2.md  +2 −0          │    └─────────────────────────────────┘                                │
```

`y` or `enter` confirms, `n` or `esc` cancels. What it restores to is the baseline lastcall
recorded for that file, which is the last state you accepted, or the committed content if
you have accepted nothing. On a file that was **added** since that baseline, putting it back
means removing it, so the hunk key asks too and the question says `Delete f1?`.

Three things to know:

- **It writes the working tree, and nothing else.** It is not an undo of an accept, it does
  not touch the index or any commit, and it leaves no `git` state behind.
- **It replaces the file rather than editing it.** An editor you have open on that file
  somewhere else is now looking at the old contents and will overwrite the restore if you
  save from it. Reload the buffer.
- **It can refuse, and says so.** A file that changed since the screen drew it, one git is
  holding open in a merge conflict, and one whose bytes git's own filters do not reproduce
  exactly (a line-ending conversion or a clean filter that is not round trip safe) are all
  refused rather than written over. The status line names the file and the reason, and the
  row stays where it was. A row the folder never read is one of those refusals: there is
  nothing recorded to put back, so it is not asked about at all and the status line says
  `not read; restore is not offered`.

## 5. Flag

When a change is wrong, `m` opens a note on the hunk:

```text
│  main · 2 files   ┌ flag hunk 1 of 1 ────────────────────────────────────────┐                   │
│  A u1  +1 −0  [ups│ f1 · hunk 1 of 1                                         │                   │
│  A u2  +2 −0  [mix│                                                          │                   │
│  upstream · 1 file│ this rewrite loses the guard                             │                   │
│───────────────────│ why?▌                                                    │                   │
│W/notes            │                                                          │                   │
│  draft · 1 file   │                                                          │                   │
│  M n2.md  +2 −0   │                                                          │                   │
│                   │                                                          │                   │
│                   │ ⏎ send   ^J newline   Esc cancel                         │                   │
│                   └──────────────────────────────────────────────────────────┘                   │
```

`enter` sends it, `ctrl-j` starts a new line inside the note, `esc` cancels. `M` clears
every flag on the file.

Where the note goes depends on what is listening. Inside a herdr session it is typed into
the agent's pane, with the cursor left after it so you press Enter yourself, and when
several agents are running in that repository lastcall asks which one instead of guessing.
With no session, the note is written to a dated file under the state directory. Either
way the flag is recorded first, so nothing is lost if the send does not happen, and the
row keeps its mark until you clear it. The details are in [`herdr.md`](herdr.md).

A flagged hunk stays visible, marked, and is not accepted by `ctrl-a`. That is the point:
the flag is a thing to come back to.

## 6. Edit

Sometimes the change is nearly right and you would rather fix it than send it back. `i`
opens the file in place, at the hunk you were reading:

```text
editing src/parse.rs · line 21/62
┌──────────────────────────┬───────────────────────────────────────────────────────────────────────┐
│alpha                     │   1▎//! A tiny line-oriented parser for `key = value` text.           │
│  main · 3 files          │   2▎//!                                                               │
│  M f1  +1 −1             │   3▎//! Comment syntax follows the sample files: `#` to the end of th→│
│  M f2  +1 −0             │   4▎                                                                  │
│  M parse.rs  +10 −2      │   5▎/// One parsed record: a key, its value, and the line it came fro→│
│──────────────────────────│   6▎#[derive(Debug, Clone, PartialEq, Eq)]                            │
│beta                      │   7 pub struct Record {                                               │
│  main · 2 files          │   8     pub key: String,                                              │
│  A u1  +1 −0  [upstream] │   9     pub value: String,                                            │
│  A u2  +2 −0  [mixed]    │  10     pub line: usize,                                              │
│  upstream · 1 file       │  11 }                                                                 │
│──────────────────────────│  12                                                                   │
│W/notes                   │  13 /// Parse `text` into one record per `key = value` line.          │
│  draft · 1 file          │  14 ///                                                               │
│  M n2.md  +2 −0          │  15 /// Blank lines and `#` comments are skipped. A line without a `=→│
│                          │  16 /// error: it is simply not a record, which keeps the parser tota→│
│                          │  17 pub fn parse(text: &str) -> Vec<Record> {                         │
│                          │  18▎    let mut out = Vec::new();                                     │
│                          │  19▎    for (i, raw) in text.lines().enumerate() {                    │
│                          │  20▎        let line = raw.trim();                                    │
│                          │  21▎        if line.is_empty() || line.starts_with('#') || line.start→│
└──────────────────────────┴───────────────────────────────────────────────────────────────────────┘
^S save   Esc close
```

The header names the file and the line you are on. The `▎` marks in the gutter are the
changed lines, so you can see what the agent touched while you are typing over it. `ctrl-s`
saves, `esc` closes and asks first if there is anything unsaved.

The save is atomic: a temporary file, flushed, given the original's permissions, then
renamed over it. An editor open on the same file elsewhere, or a crash mid-write, cannot
leave you with half a file. The saved content is also recorded as the new baseline, so your
edit is not then shown back to you as somebody else's change.

`I` hands the file to your own editor instead, at the same line, from `VISUAL` or `EDITOR`.
lastcall leaves the screen, waits, takes it back, and then asks whether to accept the file.
Answering yes blesses whatever is on disk at that moment, which is not necessarily what you
typed: if something else wrote the file after your editor exited, that is what gets
accepted. Use `I` for the things the inline editor will not hold, which is anything binary
or over the collapse size.

## 7. Copy

`v` starts a line selection in the diff, `↑` and `↓` extend it, and `y` copies. With no
selection, `y` copies the hunk under the cursor. `esc` drops the selection without leaving
the file.

```text
│──────────────────────────│@@ -51,4 +53,10 @@                      [a accept] [u restore] [m flag]│
│W/notes                   │         assert_eq!(recor copied to clipboard                          │
│  draft · 1 file          │         assert_eq!(records[0].line, 3);                               │
```

The copy goes to your terminal's clipboard through the terminal itself, which means it
works the same over ssh as it does locally, with no helper program in between. A selection
larger than about 32 KiB is refused rather than truncated, with a line saying so: a
half-pasted patch is worse than none.

Some terminals have this switched off by default, in which case nothing lands and the
terminal, not lastcall, is the place to look. `shift` and drag still selects text the
ordinary way, since the mouse is otherwise being used for clicks.

## What it remembers

Everything above is recorded outside your repositories, under
`~/.local/state/lastcall` by default: what you accepted, what you flagged, and a copy of
the baseline each file was at. Nothing is ever written inside a repository you are
watching, there is no `.lastcall` directory, and nothing you do here appears in `git
status`. Two lastcalls looking at the same repository take a lock and keep each other's
work rather than the last one winning.

What you accept is remembered per branch. Accept work on a feature branch, check out the
branch you started from, and the pile there is the one you left: the files you just accepted
are not in that branch's tree, so they are not shown as deletions you have to accept a second
time. Checking out a branch for the first time carries across what you have seen so far, so
the switch itself shows nothing new, and going back later finds that branch's own pile
waiting. A change that reaches another branch by cherry-pick shows once more on that branch,
because lastcall never guesses that you have already read it somewhere else. A detached HEAD
keeps whichever record you were on, and deleting a branch drops what it remembered.

lastcall watches the filesystem, so the screen follows an agent as it writes. Where those
events do not arrive, which happens on some network filesystems and inside some containers,
`lastcall tui --poll 2` rescans every two seconds instead. It is a fallback and not a
default: polling a large tree costs real work.

If you want the same information as text, `lastcall status` prints the pending set for
every watched repository and `lastcall status --json` prints it in a stable shape for a
script. `lastcall watch` prints one line per event as they happen. Both read the record the
screen reads.

## Related

- [`config.md`](config.md) for rebinding any of these keys, and for what to watch.
- [`herdr.md`](herdr.md) for what the screen adds when it is running next to your agents.
- [`install.md`](install.md) for getting the binary.

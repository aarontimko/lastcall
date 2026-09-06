#!/usr/bin/env bash
# The probe "$EDITOR" for Phase 8 deliverable 7. Never a real editor: the tests must not be
# able to reach one from the developer's PATH, so `PtyTui::isolated_lastcall` removes
# `$VISUAL` and `$EDITOR` from the child's environment and each editor scene points
# `$EDITOR` at an absolute path to this file (through a symlink named after the editor whose
# argv shape it should be given — `vim`, so the basename table's `+<line> <file>` is what
# lastcall builds).
#
# What it does, in order, all of it driven by the environment so one script serves every
# scene:
#
#   $LASTCALL_PROBE_EDITOR_LOG    append `argv:` (the whole argument vector) and `cwd:` (the
#                                 working directory lastcall gave the child) to this file.
#   $LASTCALL_PROBE_EDITOR_SLEEP  sleep this many seconds before doing anything else, so a
#                                 scene can type at the terminal while the "editor" owns it.
#   $LASTCALL_PROBE_EDITOR_WRITE  rewrite the file named by the **last** argument with this
#                                 content — the save a real editor would have made. Unset
#                                 means look and quit, which must leave the file alone.
#
# `set -u` and no `set -e`: a scene that interrupts the sleep with ^C expects this script to
# die from the signal, and the exit status is never asserted — lastcall treats an editor
# that exits non-zero exactly like one that exits 0, because either way the question is what
# the file holds now.
set -u

if [ -n "${LASTCALL_PROBE_EDITOR_LOG:-}" ]; then
    printf 'argv: %s\ncwd: %s\n' "$*" "$(pwd)" >>"$LASTCALL_PROBE_EDITOR_LOG"
fi

if [ -n "${LASTCALL_PROBE_EDITOR_SLEEP:-}" ]; then
    sleep "$LASTCALL_PROBE_EDITOR_SLEEP"
fi

if [ -n "${LASTCALL_PROBE_EDITOR_WRITE:-}" ]; then
    file="${*: -1}"
    printf '%s' "$LASTCALL_PROBE_EDITOR_WRITE" >"$file"
fi

exit 0

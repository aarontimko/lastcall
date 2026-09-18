#!/usr/bin/env bash
# lastcall scenario harness: the git plumbing of spec §6.1–6.5, as shell functions.
# No product code. Ledger state = files under $LC_STATE. Bash 3.2 compatible (no assoc arrays).
set -u

export GIT_AUTHOR_NAME="Me" GIT_AUTHOR_EMAIL="me@example.com"
export GIT_COMMITTER_NAME="Me" GIT_COMMITTER_EMAIL="me@example.com"
export LC_USER_EMAIL="me@example.com"

# ---------- store / ledger ----------
# lc_init R STATE KIND SCOPE   (KIND = git | draft; SCOPE = plain | tree, drafts only)
#
# SCOPE is the shape of what a watched folder covers (spec §F, Amendment v1.13): `plain` is
# the folder's own files, `tree` is the folder and everything below it. LC_DRAFT_MAX_BYTES
# is the size at which a file stops being read; it is a size, so it never decides what is
# listed, only what is hashed into the record.
lc_init() {
  LC_R="$1"; LC_STATE="$2"; LC_KIND="${3:-git}"; LC_SCOPE="${4:-plain}"
  LC_DRAFT_MAX_BYTES="${LC_DRAFT_MAX_BYTES:-524288}"
  mkdir -p "$LC_STATE/overrides"
  git init -q --bare "$LC_STATE/objects"
  if [ "$LC_KIND" = git ]; then
    local objdir; objdir="$(git -C "$LC_R" rev-parse --path-format=absolute --git-path objects)"
    mkdir -p "$LC_STATE/objects/objects/info"; echo "$objdir" > "$LC_STATE/objects/objects/info/alternates"
    # copy the config keys that affect content normalization (spec §6.4 content model)
    local k v; for k in core.autocrlf core.eol core.filemode core.ignorecase; do
      v="$(git -C "$LC_R" config --get "$k" 2>/dev/null || true)"; [ -n "$v" ] && git --git-dir="$LC_STATE/objects" config "$k" "$v"
    done
  fi
  mkdir -p "$LC_STATE/branches"
  : > "$LC_STATE/seen_tree"; : > "$LC_STATE/seen_head"; : > "$LC_STATE/seen_branch"
  : > "$LC_STATE/first_sight_head"
}
# all plumbing runs against OUR store with R as the work tree and OUR index
lcg() { GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$LC_STATE/index" git -c core.excludesfile=/dev/null "$@"; }
# the same, over a caller-named index file (the temp index of a fold)
lcgi() { local idx="$1"; shift; GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$idx" git -c core.excludesfile=/dev/null "$@"; }
rg()  { git -C "$LC_R" "$@"; }   # the user's repo
enc() { printf '%s' "$1" | sed 's|/|%2F|g'; }

lc_first_sight() {
  if [ "$LC_KIND" = git ] && rg rev-parse -q --verify HEAD >/dev/null 2>&1; then
    rg rev-parse 'HEAD^{tree}' > "$LC_STATE/seen_tree"; rg rev-parse HEAD > "$LC_STATE/seen_head"
    # R2's seen-state target: the commit the root was first sighted at, written once here
    # and never again. Empty at an unborn head and for a draft root, which makes clause (a)
    # never hold rather than making anything fail.
    rg rev-parse HEAD > "$LC_STATE/first_sight_head"
  elif [ "$LC_KIND" = draft ] && [ "${LC_DRAFT_INITIAL:-seen}" = seen ]; then
    lc_write_tree_of_disk > "$LC_STATE/seen_tree"
  else : > "$LC_STATE/seen_tree"; fi
  lc_head_branch > "$LC_STATE/seen_branch"
  lc_seed_index
}
lc_seed_index() {
  rm -f "$LC_STATE/index"
  local t; t="$(cat "$LC_STATE/seen_tree")"
  if [ -n "$t" ]; then lcg read-tree "$t"; else lcg read-tree --empty; fi
}
# Tree of the current disk content a watched folder covers (its first sight). Two rules,
# both of them here because first sight and the scan must agree about them:
#   - the shape: `plain` reads the folder's own files (`-maxdepth 1`), `tree` reads
#     everything below it, minus anything inside a repository of its own;
#   - the size: a file of LC_DRAFT_MAX_BYTES or more is not read, so `-size -Nc` (fewer
#     than N bytes) is the test, and at-or-above is out. A symlink's own size is the length
#     of its link text, which is what `find` stats without `-L`.
lc_write_tree_of_disk() {
  local tmp="$LC_STATE/index.tmp"; rm -f "$tmp"
  local depth=(); [ "${LC_SCOPE:-plain}" = plain ] && depth=(-maxdepth 1)
  ( cd "$LC_R" && find . "${depth[@]+${depth[@]}}" -name .git -prune -o \
      \( -type f -o -type l \) -size "-${LC_DRAFT_MAX_BYTES:-524288}c" -print |
    sed 's|^\./||' | sort | while read -r p; do
      GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git add -f -- "$p"; done )
  GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git write-tree
}
# The shape half of the scope, on its own: the only thing the trim (R6) is allowed to use.
lc_in_shape() {   # lc_in_shape PATH
  [ "${LC_SCOPE:-plain}" = tree ] && return 0
  case "$1" in */*) return 1;; *) return 0;; esac
}
# The size half: true when the file at PATH is small enough to be read. A path with nothing
# there is not a size question (a deletion is always a row), so it passes.
lc_small_enough() {   # lc_small_enough PATH
  [ -e "$LC_R/$1" ] || [ -L "$LC_R/$1" ] || return 0
  [ -n "$(find "$LC_R/$1" -maxdepth 0 -size "-${LC_DRAFT_MAX_BYTES:-524288}c" 2>/dev/null)" ]
}

# ---------- branches (spec §6.4 per-branch records, Amendment v1.12) ----------
# One record per branch the repo has been checked out on while lastcall watched; exactly
# one is in force (the one $LC_STATE/seen_branch names, which is the branch <git_dir>/HEAD
# names). A parked record is a directory $LC_STATE/branches/<enc name>/ holding the same
# files the record in force keeps at the top of $LC_STATE.

# R1: the branch name in <git_dir>/HEAD, or empty (detached, unborn-and-unnamed, missing,
# mid-write, or a draft root) — one file read, never a git process that resolves HEAD.
lc_head_branch() {
  [ "$LC_KIND" = git ] || return 0
  local gd; gd="$(rg rev-parse --path-format=absolute --git-dir 2>/dev/null)" || return 0
  [ -r "$gd/HEAD" ] || return 0
  sed -n 's|^ref: refs/heads/||p' "$gd/HEAD" 2>/dev/null | head -1
}
lc_parked() {   # the parked branch names, sorted, joined by |
  ls "$LC_STATE/branches" 2>/dev/null | sed 's|%2F|/|g' | LC_ALL=C sort | tr '\n' '|' | sed 's/|$//'
}
# R3: drop every parked name that has no ref. for-each-ref prints full names (%(refname),
# never %(refname:short), which a same-named tag can shadow); if it fails the prune is
# skipped for this switch, which is the fail-open side (an over-show, never a hide).
lc_branch_prune() {
  [ -d "$LC_STATE/branches" ] || return 0
  local refs; refs="$(rg for-each-ref --format='%(refname)' refs/heads 2>/dev/null)" || return 0
  local d n
  for d in "$LC_STATE/branches"/*; do
    [ -d "$d" ] || continue
    n="$(basename "$d" | sed 's|%2F|/|g')"
    printf '%s\n' "$refs" | grep -qx "refs/heads/$n" || rm -rf "$d"
  done
}
# One commit-or-tree's entry for exactly one path, as "<mode> <oid>" or ABSENT, read
# through the named runner (rg = the user's repo, lcg = our store). A path that names a
# **directory** there is ABSENT: the engine reads the whole tree with `ls-tree -r`, whose
# map holds the blobs under that name and not the name itself, and writing a 040000 entry
# into the fold's index is how the twin used to disagree with it (verifier F4).
lc_entry() {   # lc_entry <rg|lcg> <rev-or-tree> <path>
  local run="$1" rev="$2" p="$3" line mode
  line="$("$run" ls-tree "$rev" -- "$p" 2>/dev/null | head -1)"
  [ -z "$line" ] && { echo ABSENT; return; }
  mode="$(printf '%s' "$line" | awk '{print $1}')"
  [ "$mode" = 040000 ] && { echo ABSENT; return; }
  printf '%s' "$line" | awk '{print $1" "$3}'
}
# One record's composed baseline for one path, as "<mode> <oid>" or ABSENT: the override's
# blob and mode, else that record's seen tree's entry, else absent. The record is named by
# its directory, and the record in force is $LC_STATE itself — one composer for the record
# in force and for every parked record, so R2's seen-state target cannot disagree with
# itself about what a baseline is.
lc_rec_base() {   # lc_rec_base <record dir> <path>
  local d="$1" p="$2" o b m t
  o="$d/overrides/$(enc "$p")"
  if [ -f "$o" ]; then
    b="$(cat "$o")"
    [ "$b" = null ] && { echo ABSENT; return; }
    m=100644; [ -f "$o.mode" ] && m="$(cat "$o.mode")"
    echo "$m $b"; return
  fi
  t="$(cat "$d/seen_tree" 2>/dev/null)"
  # A record with no seen tree has seen nothing; without an override naming the path it
  # composes no baseline at all, never ABSENT (verifier round four, F1: read as ABSENT, a
  # parked record that had lost its tree vouched for an unaccepted deletion).
  [ -z "$t" ] && { echo UNKNOWN; return; }
  lc_entry lcg "$t" "$p"
}
# R2's second half: arriving on B from A's record, when refs/heads/A still exists and the
# two tips have a merge-base M (the same commit and "B behind A" both give M = B's head).
# Of the paths that differ between A's tip and M, a path is folded onto M's entry only when
# both hold:
#   1. the record's composed baseline for it equals A's tip entry — the user has finished
#      with that path on the branch they left;
#   2. M's entry is already seen state, which is (a) M reachable from the commit the root
#      was first sighted at, so it was committed before lastcall ever looked, or (b) some
#      other record composes exactly that entry as its own baseline.
# A folded path takes M's entry (absent at M -> removed from the record) and loses its
# override; every other differing path keeps the copy's baseline and over-shows. A gitlink
# is left alone.
lc_branch_fold() {
  local a="$1"
  rg rev-parse -q --verify "refs/heads/$a" >/dev/null 2>&1 || return 0
  local bh; bh="$(rg rev-parse -q --verify HEAD 2>/dev/null)" || return 0
  [ -n "$bh" ] || return 0
  local mb; mb="$(rg merge-base "refs/heads/$a" "$bh" 2>/dev/null)" || return 0
  [ -n "$mb" ] || return 0
  local paths; paths="$(rg diff-tree -r -z --name-only "refs/heads/$a" "$mb" 2>/dev/null | tr '\0' '\n' | grep -v '^$')"
  [ -n "$paths" ] || return 0
  # (a), asked once for the whole fold and only now that there is something to fold.
  local fsh=""; [ -f "$LC_STATE/first_sight_head" ] && fsh="$(cat "$LC_STATE/first_sight_head")"
  local cov=0
  if [ -n "$fsh" ] && rg merge-base --is-ancestor "$mb" "$fsh" >/dev/null 2>&1; then cov=1; fi
  # (b): the parked records to ask, built only when (a) did not settle the fold. The record
  # just parked is left out — it is the same record as the copy now in force, and rule 1
  # has already required base == tipA while every path here differs between tipA and M, so
  # it could never match.
  local recs="" d
  if [ "$cov" = 0 ]; then
    for d in "$LC_STATE/branches"/*; do
      [ -d "$d" ] || continue
      [ "$(basename "$d")" = "$(enc "$a")" ] && continue
      recs="$recs$d
"
    done
  fi
  local t; t="$(cat "$LC_STATE/seen_tree")"
  local folded="$LC_STATE/fold.paths" info="$LC_STATE/fold.info"
  : > "$folded"; : > "$info"
  printf '%s\n' "$paths" | while IFS= read -r p; do
    local tipm base tipa seen r
    tipm="$(lc_entry rg "$mb" "$p")"
    case "$tipm" in 160000\ *) continue;; esac
    base="$(lc_rec_base "$LC_STATE" "$p")"
    # The record in force composes against its tree, empty when it has none, as the engine's
    # Ops does; only a parked record's UNKNOWN stays UNKNOWN and matches nothing below.
    [ "$base" = UNKNOWN ] && base=ABSENT
    tipa="$(lc_entry rg "refs/heads/$a" "$p")"
    [ "$base" = "$tipa" ] || continue
    seen="$cov"
    if [ "$seen" = 0 ] && [ -n "$recs" ]; then
      while IFS= read -r r; do
        [ -n "$r" ] || continue
        [ "$(lc_rec_base "$r" "$p")" = "$tipm" ] && { seen=1; break; }
      done <<INNER
$recs
INNER
    fi
    [ "$seen" = 1 ] || continue
    printf '%s\n' "$p" >> "$folded"
    if [ "$tipm" = ABSENT ]; then printf '0 0000000000000000000000000000000000000000\t%s\n' "$p" >> "$info"
    else printf '%s\t%s\n' "$tipm" "$p" >> "$info"; fi
  done
  if [ -s "$info" ]; then
    local tmp="$LC_STATE/index.fold"; rm -f "$tmp"
    if [ -n "$t" ]; then lcgi "$tmp" read-tree "$t"; else lcgi "$tmp" read-tree --empty; fi
    lcgi "$tmp" update-index --index-info < "$info"
    lcgi "$tmp" write-tree > "$LC_STATE/seen_tree"
    while IFS= read -r p; do
      rm -f "$LC_STATE/overrides/$(enc "$p")" "$LC_STATE/overrides/$(enc "$p").mode"
    done < "$folded"
    rm -f "$tmp"
  fi
  rm -f "$folded" "$info"
}
# The sync every entry point that reads or writes state runs first (R1, R2, R3).
lc_branch_sync() {
  [ "$LC_KIND" = git ] || return 0
  local name; name="$(lc_head_branch)"
  [ -n "$name" ] || return 0                                  # R4: detached/unborn/unreadable = no switch
  local cur=""; [ -f "$LC_STATE/seen_branch" ] && cur="$(cat "$LC_STATE/seen_branch")"
  if [ -z "$cur" ]; then echo "$name" > "$LC_STATE/seen_branch"; return 0; fi   # R6: adopt, no fold
  [ "$cur" = "$name" ] && return 0
  # R3 rename: the record in force is re-labelled when its own ref is gone. No park, no
  # first sight, no fold.
  # ...and only when the name it moved to has no parked record of its own: a checkout of a
  # branch we have been on before is a switch, never a re-label, whatever became of the ref
  # we left (verifier F4).
  if ! rg rev-parse -q --verify "refs/heads/$cur" >/dev/null 2>&1 \
     && [ ! -d "$LC_STATE/branches/$(enc "$name")" ]; then
    echo "$name" > "$LC_STATE/seen_branch"; lc_branch_prune; return 0
  fi
  local pd="$LC_STATE/branches/$(enc "$cur")"                 # R3: park the record we leave
  rm -rf "$pd"; mkdir -p "$pd/overrides"
  cp "$LC_STATE/seen_tree" "$pd/seen_tree"; cp "$LC_STATE/seen_head" "$pd/seen_head"
  cp "$LC_STATE"/overrides/* "$pd/overrides/" 2>/dev/null || true
  local nd="$LC_STATE/branches/$(enc "$name")"
  rm -f "$LC_STATE"/overrides/* 2>/dev/null || true
  if [ -d "$nd" ]; then                                       # R3: the parked record comes back
    cp "$nd/seen_tree" "$LC_STATE/seen_tree"; cp "$nd/seen_head" "$LC_STATE/seen_head"
    cp "$nd/overrides/"* "$LC_STATE/overrides/" 2>/dev/null || true
    rm -rf "$nd"
  else                                                        # R2: a copy of the record just left...
    cp "$pd/seen_tree" "$LC_STATE/seen_tree"
    cp "$pd/overrides/"* "$LC_STATE/overrides/" 2>/dev/null || true
    rg rev-parse HEAD > "$LC_STATE/seen_head" 2>/dev/null || : > "$LC_STATE/seen_head"
    lc_branch_fold "$cur"                                     # ...then the ancestor fold
  fi
  echo "$name" > "$LC_STATE/seen_branch"
  lc_branch_prune
  lc_seed_index
}

# ---------- content ----------
lc_hash() {   # current content oid of path (through clean filter), or ABSENT; symlink = link text
  local p="$1"
  if [ -L "$LC_R/$p" ]; then readlink "$LC_R/$p" | lcg hash-object -w --stdin
  elif [ -f "$LC_R/$p" ]; then (cd "$LC_R" && lcg hash-object -w -- "$p")
  else echo ABSENT; fi
}
lc_mode() { local p="$1"; if [ -L "$LC_R/$p" ]; then echo 120000; elif [ -x "$LC_R/$p" ] && [ "$(lcg config --get core.filemode || echo true)" != false ]; then echo 100755; elif [ -f "$LC_R/$p" ]; then echo 100644; else echo ABSENT; fi; }
lc_tree_blob() { local t; t="$(cat "$LC_STATE/seen_tree")"; [ -z "$t" ] && { echo EMPTY; return; }
  lcg ls-tree "$t" -- "$1" | awk '{print ($3==""?"EMPTY":$3)}' | { read -r x; echo "${x:-EMPTY}"; }; }
lc_tree_mode() { local t; t="$(cat "$LC_STATE/seen_tree")"; [ -z "$t" ] && { echo EMPTY; return; }
  lcg ls-tree "$t" -- "$1" | awk '{print $1}' | { read -r x; echo "${x:-EMPTY}"; }; }
lc_baseline() {  # override blob | tree blob | EMPTY   ("null" override = ABSENT)
  lc_branch_sync
  local o="$LC_STATE/overrides/$(enc "$1")"
  if [ -f "$o" ]; then local b; b="$(cat "$o")"; [ "$b" = null ] && echo ABSENT || echo "$b"; else lc_tree_blob "$1"; fi
}
lc_verify_oid() { lcg cat-file -e "$1" 2>/dev/null; }

# ---------- scan ----------
# The reader's note on a path, and nothing else: an override that carries no baseline, so
# the path's baseline is still whatever the seen tree says (ledger `blob` absent).
lc_flag() {   # lc_flag PATH
  echo flag > "$LC_STATE/overrides/$(enc "$1")"; rm -f "$LC_STATE/overrides/$(enc "$1").mode"; }
# Does this path's override carry a baseline of its own, that is a blob or the `null` that
# lets the path go? A flag-only override carries neither.
lc_override_has_baseline() {   # lc_override_has_baseline PATH
  local o="$LC_STATE/overrides/$(enc "$1")" b
  [ -f "$o" ] || return 1
  b="$(cat "$o")"
  [ "$b" = null ] && return 0
  printf '%s' "$b" | grep -qE '^[0-9a-f]{40}$'
}
# Does the record hold content for this path? The override's blob first (`null` is the
# record letting the path go, which holds nothing), then the seen tree.
lc_record_holds() {   # lc_record_holds PATH
  local p="$1" o="$LC_STATE/overrides/$(enc "$p")"
  if [ -f "$o" ]; then [ "$(cat "$o")" != null ]; return; fi
  local t; t="$(cat "$LC_STATE/seen_tree")"
  [ -n "$t" ] || return 1
  [ -n "$(lcg ls-tree "$t" -- "$p" 2>/dev/null)" ]
}
# The scope filter of a watched folder's scan: the shape decides what the folder covers,
# and the size decides only what may be *read*, so a path the record already holds stays a
# candidate whatever its size (a recorded file that grew past the threshold is still a row,
# which says it was not read). A large file the record does not hold is counted, not listed.
lc_draft_candidate() {   # lc_draft_candidate PATH
  lc_in_shape "$1" || return 1
  lc_small_enough "$1" && return 0
  lc_record_holds "$1"
}
lc_raw_candidates() {   # the three sources, before the scope filter
  lcg update-index -q --refresh --ignore-submodules >/dev/null 2>&1 || true
  { lcg diff-files --name-only -z | tr '\0' '\n'
    if [ "$LC_KIND" = git ]; then lcg ls-files --others --exclude-standard -z | tr '\0' '\n'
    else lcg ls-files --others -z | tr '\0' '\n'; fi
    ls "$LC_STATE/overrides" 2>/dev/null | sed 's|%2F|/|g;s|\.mode$||'
  } | grep -v '^$' | sort -u
}
lc_candidates() {
  lc_raw_candidates |
    { if [ "$LC_KIND" = draft ]; then
        while IFS= read -r p; do lc_draft_candidate "$p" && printf '%s\n' "$p"; done
      else cat; fi; } |
    { if [ "$LC_KIND" = git ]; then rg ls-files -v -z 2>/dev/null | tr '\0' '\n' | awk '/^S /{print substr($0,3)}' > "$LC_STATE/skip.tmp"; grep -vxF -f "$LC_STATE/skip.tmp" 2>/dev/null || cat; else cat; fi; }
}
# The paths a scan counts instead of listing: inside the folder's shape, too big to read,
# and not held by the record. One line in the pile says how many there are.
lc_unread_count() {
  [ "$LC_KIND" = draft ] || { echo 0; return; }
  lc_raw_candidates | { local n=0 p
    while IFS= read -r p; do
      lc_in_shape "$p" || continue
      lc_small_enough "$p" && continue
      lc_record_holds "$p" && continue
      n=$((n + 1))
    done; echo "$n"; }
}
# R6's trim: the record of a watched folder loses every path the folder's shape no longer
# covers, by shape alone and never by size. Prints the number of paths dropped. The time
# the folder was last reviewed is not touched, and nothing is dropped when everything the
# record holds is already inside the scope.
lc_scope_trim() {
  local t; t="$(cat "$LC_STATE/seen_tree")"
  local out="" p
  if [ -n "$t" ]; then
    while IFS= read -r p; do
      [ -n "$p" ] || continue
      lc_in_shape "$p" || out="$out$p
"
    done <<TREE
$(lcg ls-tree -r --name-only "$t")
TREE
  fi
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    lc_in_shape "$p" && continue
    # A flag-only override is the reader's note, not a baseline: there is nothing for the
    # trim to drop at that path, so it is neither dropped nor counted (verifier F5).
    lc_override_has_baseline "$p" || continue
    out="$out$p
"
  done <<OVER
$(ls "$LC_STATE/overrides" 2>/dev/null | sed 's|\.mode$||;s|%2F|/|g' | sort -u)
OVER
  out="$(printf '%s' "$out" | grep -v '^$' | sort -u)"
  [ -n "$out" ] || { echo 0; return; }
  if [ -n "$t" ]; then
    local tmp="$LC_STATE/index.trim"; rm -f "$tmp"
    lcgi "$tmp" read-tree "$t"
    printf '%s\n' "$out" | while IFS= read -r p; do
      printf '0 0000000000000000000000000000000000000000\t%s\n' "$p"
    done | lcgi "$tmp" update-index --index-info
    lcgi "$tmp" write-tree > "$LC_STATE/seen_tree"
    rm -f "$tmp"
  fi
  printf '%s\n' "$out" | while IFS= read -r p; do
    # The path leaves the record, but the note on it stays: a flag-only override survives
    # the trim, here as in the engine (verifier F5).
    lc_override_has_baseline "$p" || continue
    rm -f "$LC_STATE/overrides/$(enc "$p")" "$LC_STATE/overrides/$(enc "$p").mode"
  done
  lc_seed_index
  printf '%s\n' "$out" | grep -c .
}
lc_pending_p() {   # prints "path" if pending (content OR mode differs), with resolution failure -> over-show
  local p="$1" base cur bm cm
  base="$(lc_baseline "$p")"; cur="$(lc_hash "$p")"
  case "$base" in EMPTY) base=ABSENT;; esac
  if [ "$base" != ABSENT ] && ! lc_verify_oid "$base"; then base="$(lc_tree_blob "$p")"; [ "$base" = EMPTY ] && base=ABSENT; lc_verify_oid "$base" 2>/dev/null || base=ABSENT; fi
  if [ "$base" != "$cur" ]; then echo "$p"; return; fi
  cm="$(lc_mode "$p")"; bm="$(lc_tree_mode "$p")"; local o="$LC_STATE/overrides/$(enc "$p").mode"; [ -f "$o" ] && bm="$(cat "$o")"
  [ "$cm" != ABSENT ] && [ "$bm" != EMPTY ] && [ "$cm" != "$bm" ] && echo "$p"
}
# upstream classification (§6.4): range seen_head..HEAD (merge-base fallback); U = remote-reachable AND not ours
lc_upstream_paths() {   # emits lines "path U" (changed only by upstream commits) / "path M" (touched by both sides)
  [ "$LC_KIND" = git ] || return 0
  local sh; sh="$(cat "$LC_STATE/seen_head")"; [ -z "$sh" ] && return 0
  local heads h; heads="$(rg rev-parse HEAD 2>/dev/null)" || return 0
  local mh; mh="$(rg rev-parse -q --verify MERGE_HEAD 2>/dev/null)" && heads="$heads $mh"
  local tmpu="$LC_STATE/u.tmp" tmpl="$LC_STATE/l.tmp" tmpm="$LC_STATE/m.tmp"; : > "$tmpu"; : > "$tmpl"; : > "$tmpm"
  for h in $heads; do
    local base; if rg merge-base --is-ancestor "$sh" "$h" 2>/dev/null; then base="$sh"; else base="$(rg merge-base "$sh" "$h" 2>/dev/null)" || continue; fi
    [ -z "$base" ] && continue
    local c; for c in $(rg rev-list "$base..$h"); do
      local remote=0 mine=0 ismerge=0
      rg rev-list -n1 "$c" --not --remotes | grep -q . || remote=1
      [ "$(rg log -1 --format='%ae %ce' "$c" | tr ' ' '\n' | grep -c "^$LC_USER_EMAIL$")" -gt 0 ] && mine=1
      [ "$(rg rev-list --parents -n1 "$c" | wc -w | tr -d ' ')" -gt 2 ] && ismerge=1
      local paths; paths="$(rg diff-tree --no-commit-id -r --name-only --cc "$c")"
      if [ $remote = 1 ] && [ $mine = 0 ]; then echo "$paths" >> "$tmpu"
      elif [ $ismerge = 1 ]; then echo "$paths" >> "$tmpm"      # local merge: --cc = paths both sides touched
      else echo "$paths" >> "$tmpl"; fi
    done
  done
  sort -u "$tmpu" | grep -v '^$' | while read -r p; do
    if grep -qx "$p" "$tmpl" || grep -qx "$p" "$tmpm"; then echo "$p M"; else echo "$p U"; fi; done
}
lc_pile() {   # sorted lines: "path" or "path upstream" or "path mixed"
  lc_branch_sync
  local ups="$LC_STATE/ups.tmp"; lc_upstream_paths > "$ups"
  lc_candidates | while read -r p; do
    [ -n "$(lc_pending_p "$p")" ] || continue
    local cls; cls="$(awk -v P="$p" '$1==P{print $2}' "$ups")"
    if [ "$cls" = U ]; then
      local cur hb ok=0; cur="$(lc_hash "$p")"
      for h in HEAD MERGE_HEAD; do hb="$(rg rev-parse -q --verify "$h:$p" 2>/dev/null)" && [ "$cur" = "$hb" ] && ok=1; done
      if [ $ok = 1 ]; then echo "$p upstream"; else echo "$p mixed"; fi
    elif [ "$cls" = M ]; then echo "$p mixed"
    else echo "$p"; fi
  done | sort
}

# ---------- operations ----------
lc_accept_file() { lc_branch_sync; local p="$1" cur; cur="$(lc_hash "$p")"   # (harness: rendered == live unless caller passes a blob)
  [ -n "${2:-}" ] && cur="$2"
  if [ "$cur" = ABSENT ]; then echo null > "$LC_STATE/overrides/$(enc "$p")"; else echo "$cur" > "$LC_STATE/overrides/$(enc "$p")"; lc_mode "$p" > "$LC_STATE/overrides/$(enc "$p").mode"; fi
  [ "$cur" = "$(lc_tree_blob "$p")" ] && [ "$(lc_mode "$p")" = "$(lc_tree_mode "$p")" ] && rm -f "$LC_STATE/overrides/$(enc "$p")" "$LC_STATE/overrides/$(enc "$p").mode"; true; }
# Accepting the row of a file that was not read: the record lets the path go and the file
# stays on disk untouched. There is no content CAS, because not reading it is the point.
lc_accept_unread() { lc_branch_sync; local p="$1"
  echo null > "$LC_STATE/overrides/$(enc "$p")"; rm -f "$LC_STATE/overrides/$(enc "$p").mode"; }
lc_accept_all() {   # build tree from rendered blobs: baseline-composed = current content for pending, else baseline
  lc_branch_sync
  local tmp="$LC_STATE/index.tmp"; rm -f "$tmp"
  local t; t="$(cat "$LC_STATE/seen_tree")"
  if [ -n "$t" ]; then GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git read-tree "$t"; else GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git read-tree --empty; fi
  local p cur; lc_candidates | while read -r p; do
    cur="$(lc_hash "$p")"; local f="$LC_STATE/rendered/$(enc "$p")"; [ -f "$f" ] && cur="$(cat "$f")"   # CAS snapshot wins
    if [ "$cur" = ABSENT ]; then printf '0 0000000000000000000000000000000000000000\t%s\n' "$p"
    else printf '%s %s\t%s\n' "$(lc_mode "$p")" "$cur" "$p"; fi
  done | GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git update-index --index-info
  GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git write-tree > "$LC_STATE/seen_tree"
  rm -f "$LC_STATE"/overrides/* "$LC_STATE"/rendered/* 2>/dev/null; rg rev-parse HEAD > "$LC_STATE/seen_head" 2>/dev/null || true
  lc_seed_index
}
lc_snapshot_rendered() { mkdir -p "$LC_STATE/rendered"; local p; for p in "$@"; do lc_hash "$p" > "$LC_STATE/rendered/$(enc "$p")"; done; }
lc_restart() { lc_branch_sync; lc_seed_index; }   # everything else is recomputed (an offline switch lands here)

# ---------- discovery (search_depth, D12) ----------
# lc_discover PARENT DEPTH -> the directories holding a .git entry that discovery lists at
# that depth, parent-relative, one per line, sorted by bytes.
#
# The rules the engine's walk follows, in find's vocabulary: -maxdepth caps the level (a
# root at level N has its .git entry at find depth N+1); the skip names are pruned before
# anything under them is read; .git is pruned so nothing inside one is read; find does not
# follow symlinks without -L, so a symlinked directory is neither read nor listed; and the
# awk pass drops any candidate under another, which is how "the walk never enters a
# repository" reads as output (a submodule or a vendored clone inside a root is not listed).
lc_discover() {
  local p="$1" n="$2"
  ( cd "$p" && find . -maxdepth $((n + 1)) \
      \( -name node_modules -o -name target -o -name .venv -o -name vendor \) -prune -o \
      -name .git -print -prune ) |
    sed 's|^\./||' | grep '/\.git$' | sed 's|/\.git$||' | LC_ALL=C sort |
    awk '{ keep = 1; for (r in kept) if (index($0, r "/") == 1) keep = 0
           if (keep) { kept[$0] = 1; print } }'
}

# ---------- assertions ----------
PASS=0; FAIL=0
assert_pile() {   # assert_pile "scenario" "expected lines joined by |"
  local name="$1" exp="$2" got; got="$(lc_pile | tr '\n' '|' | sed 's/|$//')"
  if [ "$got" = "$exp" ]; then PASS=$((PASS+1)); echo "  ok   $name"; else FAIL=$((FAIL+1)); echo "  FAIL $name"; echo "       expected: [$exp]"; echo "       got:      [$got]"; fi
}
assert_str() {   # assert_str "scenario" "expected" "got"
  local name="$1" exp="$2" got="$3"
  if [ "$got" = "$exp" ]; then PASS=$((PASS+1)); echo "  ok   $name"; else FAIL=$((FAIL+1)); echo "  FAIL $name"; echo "       expected: [$exp]"; echo "       got:      [$got]"; fi
}
assert_roots() {   # assert_roots "scenario" "expected joined by |" PARENT DEPTH
  local name="$1" exp="$2" got; got="$(lc_discover "$3" "$4" | tr '\n' '|' | sed 's/|$//')"
  if [ "$got" = "$exp" ]; then PASS=$((PASS+1)); echo "  ok   $name"; else FAIL=$((FAIL+1)); echo "  FAIL $name"; echo "       expected: [$exp]"; echo "       got:      [$got]"; fi
}

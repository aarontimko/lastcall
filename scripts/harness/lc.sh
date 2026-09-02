#!/usr/bin/env bash
# lastcall scenario harness: the git plumbing of spec §6.1–6.5, as shell functions.
# No product code. Ledger state = files under $LC_STATE. Bash 3.2 compatible (no assoc arrays).
set -u

export GIT_AUTHOR_NAME="Me" GIT_AUTHOR_EMAIL="me@example.com"
export GIT_COMMITTER_NAME="Me" GIT_COMMITTER_EMAIL="me@example.com"
export LC_USER_EMAIL="me@example.com"

# ---------- store / ledger ----------
# lc_init R STATE KIND   (KIND = git | draft)
lc_init() {
  LC_R="$1"; LC_STATE="$2"; LC_KIND="${3:-git}"
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
  : > "$LC_STATE/seen_tree"; : > "$LC_STATE/seen_head"
}
# all plumbing runs against OUR store with R as the work tree and OUR index
lcg() { GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$LC_STATE/index" git -c core.excludesfile=/dev/null "$@"; }
rg()  { git -C "$LC_R" "$@"; }   # the user's repo
enc() { printf '%s' "$1" | sed 's|/|%2F|g'; }

lc_first_sight() {
  if [ "$LC_KIND" = git ] && rg rev-parse -q --verify HEAD >/dev/null 2>&1; then
    rg rev-parse 'HEAD^{tree}' > "$LC_STATE/seen_tree"; rg rev-parse HEAD > "$LC_STATE/seen_head"
  elif [ "$LC_KIND" = draft ] && [ "${LC_DRAFT_INITIAL:-seen}" = seen ]; then
    lc_write_tree_of_disk > "$LC_STATE/seen_tree"
  else : > "$LC_STATE/seen_tree"; fi
  lc_seed_index
}
lc_seed_index() {
  rm -f "$LC_STATE/index"
  local t; t="$(cat "$LC_STATE/seen_tree")"
  if [ -n "$t" ]; then lcg read-tree "$t"; else lcg read-tree --empty; fi
}
lc_write_tree_of_disk() {   # tree of current disk content (draft first sight)
  local tmp="$LC_STATE/index.tmp"; rm -f "$tmp"
  ( cd "$LC_R" && find . -type f ! -path './.git/*' | sed 's|^\./||' | sort | while read -r p; do
      GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git add -f -- "$p"; done )
  GIT_DIR="$LC_STATE/objects" GIT_WORK_TREE="$LC_R" GIT_INDEX_FILE="$tmp" git write-tree
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
  local o="$LC_STATE/overrides/$(enc "$1")"
  if [ -f "$o" ]; then local b; b="$(cat "$o")"; [ "$b" = null ] && echo ABSENT || echo "$b"; else lc_tree_blob "$1"; fi
}
lc_verify_oid() { lcg cat-file -e "$1" 2>/dev/null; }

# ---------- scan ----------
lc_candidates() {
  lcg update-index -q --refresh --ignore-submodules >/dev/null 2>&1 || true
  { lcg diff-files --name-only -z | tr '\0' '\n'
    if [ "$LC_KIND" = git ]; then lcg ls-files --others --exclude-standard -z | tr '\0' '\n'
    else lcg ls-files --others -z | tr '\0' '\n'; fi
    ls "$LC_STATE/overrides" 2>/dev/null | sed 's|%2F|/|g'
  } | grep -v '^$' | sort -u | { if [ "$LC_KIND" = git ]; then rg ls-files -v -z 2>/dev/null | tr '\0' '\n' | awk '/^S /{print substr($0,3)}' > "$LC_STATE/skip.tmp"; grep -vxF -f "$LC_STATE/skip.tmp" 2>/dev/null || cat; else cat; fi; }
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
lc_accept_file() { local p="$1" cur; cur="$(lc_hash "$p")"   # (harness: rendered == live unless caller passes a blob)
  [ -n "${2:-}" ] && cur="$2"
  if [ "$cur" = ABSENT ]; then echo null > "$LC_STATE/overrides/$(enc "$p")"; else echo "$cur" > "$LC_STATE/overrides/$(enc "$p")"; lc_mode "$p" > "$LC_STATE/overrides/$(enc "$p").mode"; fi
  [ "$cur" = "$(lc_tree_blob "$p")" ] && [ "$(lc_mode "$p")" = "$(lc_tree_mode "$p")" ] && rm -f "$LC_STATE/overrides/$(enc "$p")" "$LC_STATE/overrides/$(enc "$p").mode"; true; }
lc_accept_all() {   # build tree from rendered blobs: baseline-composed = current content for pending, else baseline
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
lc_restart() { lc_seed_index; }   # everything else is recomputed

# ---------- assertions ----------
PASS=0; FAIL=0
assert_pile() {   # assert_pile "scenario" "expected lines joined by |"
  local name="$1" exp="$2" got; got="$(lc_pile | tr '\n' '|' | sed 's/|$//')"
  if [ "$got" = "$exp" ]; then PASS=$((PASS+1)); echo "  ok   $name"; else FAIL=$((FAIL+1)); echo "  FAIL $name"; echo "       expected: [$exp]"; echo "       got:      [$got]"; fi
}

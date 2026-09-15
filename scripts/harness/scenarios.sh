#!/usr/bin/env bash
set -u
H="$(cd "$(dirname "$0")" && pwd)"; . "$H/lc.sh"
W="${LC_WORK:-$H/work}"; rm -rf "$W"; mkdir -p "$W"   # override with LC_WORK=/tmp/x; default dir is gitignored
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
OTHER_A="-c user.name=Coworker -c user.email=coworker@example.com"

# ---- fixtures ----
mk_repo() {  # mk_repo NAME -> repo at $W/NAME with origin bare at $W/NAME.git, 3 files committed & pushed
  local n="$1" r="$W/$1"; git init -q -b main "$r"; git init -q --bare -b main "$W/$n.git"
  git -C "$r" remote add origin "$W/$n.git"
  printf 'a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n' > "$r/f1"; printf 'b\n' > "$r/f2"; printf 'c\n' > "$r/f3"
  git -C "$r" add -A && git -C "$r" commit -q -m init && git -C "$r" push -q -u origin main
  echo "$r"
}
# coworker pushes N files to origin/main from a separate clone
coworker_push() { local n="$1" count="$2" prefix="${3:-u}"; local c="$W/$n.cw"; rm -rf "$c"
  git clone -q "$W/$n.git" "$c"; local i; for i in $(seq 1 "$count"); do echo "up$i" > "$c/$prefix$i"; done
  git -C "$c" add -A; GIT_AUTHOR_EMAIL=coworker@example.com GIT_COMMITTER_EMAIL=coworker@example.com git -C "$c" commit -q -m "coworker $count files"; git -C "$c" push -q origin main; }
fresh() { local n="$1"; R="$(mk_repo "$n")"; S="$W/$n.state"; rm -rf "$S"; mkdir -p "$S"; lc_init "$R" "$S" git; lc_first_sight; }

echo "== A. core =="
fresh a1; echo "a1 CHANGED" > "$R/f1".tmp && mv "$R/f1.tmp" "$R/f1"
assert_pile "A1 first sight: uncommitted edit pending" "f1"
lc_restart; assert_pile "A1 restart" "f1"
lc_accept_file f1; assert_pile "A2 accept file -> empty" ""
lc_restart; assert_pile "A2 restart -> empty" ""
printf 'a1 CHANGED\nmore\n' > "$R/f1"; assert_pile "A2 further edit pending vs override" "f1"
echo x > "$R/f2"; echo y > "$R/new1"; lc_accept_all; assert_pile "A4 accept all -> empty" ""
lc_restart; assert_pile "A4 restart" ""
[ "$(lcg ls-tree "$(cat "$S/seen_tree")" -- new1 | wc -l | tr -d ' ')" = 1 ] && echo "  ok   A4 seen tree contains new1" || echo "  FAIL A4 tree missing new1"
# A5: accept-all CAS — rendered snapshot taken, then disk changes before confirm
echo r1 > "$R/f2"; lc_snapshot_rendered f2; echo r2 > "$R/f2"; lc_accept_all
assert_pile "A5 accept-all blesses rendered f2, live delta pending" "f2"
# A7: deletion
fresh a7; rm "$R/f3"; assert_pile "A7 deletion pending" "f3"
lc_accept_file f3; assert_pile "A7 accept deletion -> empty" ""
printf 'c\n' > "$R/f3"; assert_pile "A7 recreate after accepted deletion -> pending" "f3"

echo "== B. history =="
fresh b1; echo "edit" >> "$R/f1"; assert_pile "B1 pre-commit" "f1"
git -C "$R" commit -qam "agent commit"; assert_pile "B1 commit does NOT clear pending" "f1"
lc_restart; assert_pile "B1 restart" "f1"
git -C "$R" checkout -q -b feat-x; assert_pile "B2 checkout -b: unchanged" "f1"
git -C "$R" checkout -q main; git -C "$R" checkout -q feat-x; assert_pile "B3 flip-flop: unchanged" "f1"
# B9: push the agent commit — must stay an individual row (not grouped as upstream)
git -C "$R" push -q -u origin feat-x; assert_pile "B9 push: still individual pending" "f1"
# B4: divergent branch and back, with an override on f1
fresh b4; git -C "$R" checkout -q -b feat-y; echo g1 > "$R/g1"; echo g2 > "$R/g2"; git -C "$R" add -A; git -C "$R" commit -qm feat; git -C "$R" checkout -q main
echo "edit" >> "$R/f1"; lc_accept_file f1; assert_pile "B4 baseline: override on f1, empty pile" ""
git -C "$R" checkout -q feat-y; assert_pile "B4 switch to feat-y over-shows" "g1|g2"
git -C "$R" checkout -q main; assert_pile "B4 switch back self-clears, override intact" ""
# B5: rebase preserving content; upstream touched u1
fresh b5; git -C "$R" checkout -q -b feat-x; echo l1 > "$R/l1"; git -C "$R" add -A; git -C "$R" commit -qm local1; echo l2 > "$R/l2"; git -C "$R" add -A; git -C "$R" commit -qm local2
lc_accept_all; assert_pile "B5 accepted at feat-x tip" ""
coworker_push b5 1; git -C "$R" fetch -q; git -C "$R" rebase -q origin/main
assert_pile "B5 rebase: only upstream group, rebased commits re-flag nothing" "u1 upstream"
# B6: amend changing content
fresh b6; echo v1 > "$R/f2"; git -C "$R" commit -qam v1; lc_accept_all; echo v2 > "$R/f2"; git -C "$R" commit -q --amend -am v2
assert_pile "B6 amend: exactly the amended delta" "f2"
# B8: stash / pop
fresh b8; echo edit >> "$R/f1"; assert_pile "B8 pending" "f1"; git -C "$R" stash -q; assert_pile "B8 stash -> not on disk, not pending" ""
git -C "$R" stash pop -q; assert_pile "B8 pop -> back" "f1"
# B7: reset --hard on accepted work
fresh b7; echo t > "$R/f2"; git -C "$R" commit -qam T; lc_accept_all; git -C "$R" reset -q --hard HEAD~1
assert_pile "B7 reset: reverse delta over-shows" "f2"

echo "== C. upstream =="
fresh c1; coworker_push c1 3; git -C "$R" fetch -q; assert_pile "C1 fetch only: unchanged" ""
git -C "$R" pull -q --ff-only; assert_pile "C2 ff pull: grouped" "u1 upstream|u2 upstream|u3 upstream"
lc_accept_all; assert_pile "C2 accept group -> empty" ""
# C3: sponsor scenario
fresh c3; git -C "$R" checkout -q -b feat-x; echo fx > "$R/fx"; git -C "$R" add -A; git -C "$R" commit -qm fx; lc_accept_all
echo p > "$R/parse.rs"; echo l > "$R/lexer.rs"; coworker_push c3 4; git -C "$R" fetch -q
git -C "$R" merge -q --no-edit origin/main; assert_pile "C3 merge: uncommitted individual, upstream grouped, merge commit adds nothing" "lexer.rs|parse.rs|u1 upstream|u2 upstream|u3 upstream|u4 upstream"
lc_restart; assert_pile "C3 restart" "lexer.rs|parse.rs|u1 upstream|u2 upstream|u3 upstream|u4 upstream"
# C4: conflicts resolved
fresh c4; git -C "$R" checkout -q -b feat-x; echo mine > "$R/f2"; git -C "$R" commit -qam mine; lc_accept_all
c="$W/c4.cw"; rm -rf "$c"; git clone -q "$W/c4.git" "$c"; echo theirs > "$c/f2"; echo other > "$c/u1"; git -C "$c" add -A; GIT_AUTHOR_EMAIL=coworker@example.com GIT_COMMITTER_EMAIL=coworker@example.com git -C "$c" commit -qm cw; git -C "$c" push -q origin main
git -C "$R" fetch -q; git -C "$R" merge -q --no-edit origin/main >/dev/null 2>&1 || true
assert_pile "C4 during conflict: f2 with markers (mixed), u1 grouped" "f2 mixed|u1 upstream"
echo resolved > "$R/f2"; git -C "$R" add f2; git -C "$R" commit -qm merge >/dev/null
assert_pile "C4 after resolution: resolution individual w/ badge, rest grouped" "f2 mixed|u1 upstream"
# C5: both sides touched the same file (different regions)
fresh c5; git -C "$R" checkout -q -b feat-x; sed -i '' 's/^a1$/A1/' "$R/f1"; git -C "$R" commit -qam local; lc_accept_all
c="$W/c5.cw"; rm -rf "$c"; git clone -q "$W/c5.git" "$c"; sed -i '' 's/^a10$/A10/' "$c/f1"; GIT_AUTHOR_EMAIL=coworker@example.com GIT_COMMITTER_EMAIL=coworker@example.com git -C "$c" commit -qam cw; git -C "$c" push -q origin main
git -C "$R" fetch -q; git -C "$R" merge -q --no-edit origin/main
assert_pile "C5 both sides: individual row, mixed badge" "f1 mixed"
# C6: upstream file then agent edit on top
fresh c6; coworker_push c6 2; git -C "$R" pull -q --ff-only; echo more >> "$R/u2"
assert_pile "C6 upstream + uncommitted delta = mixed" "u1 upstream|u2 mixed"

echo "== D. content =="
fresh d1; chmod +x "$R/f2"; assert_pile "D1 mode-only change pending" "f2"; lc_accept_file f2; assert_pile "D1 accept records mode" ""
fresh d3; printf '* text=auto\n' > "$R/.gitattributes"; git -C "$R" add -A; git -C "$R" commit -qm attrs; lc_first_sight
printf 'l1\r\nl2\r\nl3\r\n' > "$R/crlf.txt"; git -C "$R" add crlf.txt; git -C "$R" commit -qm crlf; lc_first_sight
printf 'l1\r\nl2 changed\r\nl3\r\n' > "$R/crlf.txt"
assert_pile "D3 CRLF: pending" "crlf.txt"
base="$(lc_baseline crlf.txt)"; cur="$(lc_hash crlf.txt)"; n="$(lcg diff --numstat "$base" "$cur" | awk '{print $1"/"$2}')"
[ "$n" = "1/1" ] && echo "  ok   D3 numstat 1/1 (normalized, no whole-file churn)" || { echo "  FAIL D3 numstat $n"; FAIL=$((FAIL+1)); }
fresh d6; mkdir -p "$R/src" "$R/other"; echo s > "$R/src/s"; echo o > "$R/other/o"; git -C "$R" add -A; git -C "$R" commit -qm dirs; lc_first_sight
git -C "$R" sparse-checkout set --no-cone src >/dev/null 2>&1; [ -e "$R/other/o" ] && echo "  (sparse did not remove other/o; skipping D6)" || assert_pile "D6 sparse: no false deletions" ""
fresh d10; git -C "$R" worktree add -q "$W/d10-wt" -b feat-w; R2="$W/d10-wt"; S2="$W/d10-wt.state"; mkdir -p "$S2"
( LC_R="$R2"; LC_STATE="$S2"; lc_init "$R2" "$S2" git; lc_first_sight; echo e >> "$R2/f1"; git -C "$R2" commit -qam wt; assert_pile "D10 linked worktree: commit keeps pending" "f1" )
fresh d11; mkdir -p "$R/docs"; printf 'x\n' > "$R/docs/résumé draft.md"; assert_pile "D11 unicode+space path" "docs/résumé draft.md"
# D12: search_depth reads N folders down. The fixture is the scenario's, built once.
P="$W/d12"; mkdir -p "$P"
git init -q -b main "$P/a"; echo one > "$P/a/f1"; git -C "$P/a" add -A; git -C "$P/a" commit -qm init
git init -q -b main "$P/a/sub"; echo s > "$P/a/sub/s"; git -C "$P/a/sub" add -A; git -C "$P/a/sub" commit -qm sub
git -C "$P/a" add sub 2>/dev/null; git -C "$P/a" commit -qm gitlink            # a committed submodule of a
mkdir -p "$P/worktrees"; git clone -q "$P/a" "$P/worktrees/b"      # a clone kept one folder down
git -C "$P/a" worktree add -q "$P/worktrees/a-wt" -b feat-w        # a linked worktree beside it
mkdir -p "$P/deep/er"; git init -q -b main "$P/deep/er/c"          # two folders down
mkdir -p "$P/node_modules"; git init -q -b main "$P/node_modules/pkg"
ln -s "$P/deep" "$P/link"
assert_roots "D12 depth 1: the repositories directly inside the parent" "a" "$P" 1
assert_roots "D12 depth 2: the folder below too, submodule and node_modules and link never" "a|worktrees/a-wt|worktrees/b" "$P" 2
assert_roots "D12 depth 3: two folders below" "a|deep/er/c|worktrees/a-wt|worktrees/b" "$P" 3

echo "== E. storage =="
fresh e2; echo edit >> "$R/f1"; lc_accept_file f1; echo deadbeefdeadbeefdeadbeefdeadbeefdeadbeef > "$S/overrides/f1"
assert_pile "E2 corrupt override -> falls to tree baseline (over-show)" "f1"
fresh e5; for i in $(seq 1 12); do echo v > "$R/o$i"; lc_accept_file "o$i"; done; before="$(lc_pile)"; lc_accept_all; [ "$(lc_pile)" = "$before" ] && echo "  ok   E5 compaction preserves pile" || echo "  FAIL E5"

echo "== F. drafts =="
mkdir -p "$W/notes"; for i in $(seq 1 5); do echo n > "$W/notes/n$i.md"; done
S="$W/notes.state"; mkdir -p "$S"; lc_init "$W/notes" "$S" draft; lc_first_sight; assert_pile "F3 non-git draft dir first sight (seen)" ""
echo changed > "$W/notes/n2.md"; echo new > "$W/notes/n9.md"; assert_pile "F3 draft edits pending" "n2.md|n9.md"; lc_accept_all; assert_pile "F3 accept all" ""
LC_DRAFT_INITIAL=pending; S="$W/notes2.state"; mkdir -p "$S" "$W/notes2"; echo a > "$W/notes2/a"; lc_init "$W/notes2" "$S" draft; lc_first_sight; assert_pile "F2 draft_initial=pending" "a"; unset LC_DRAFT_INITIAL

echo; echo "PASS=$PASS FAIL=$FAIL"

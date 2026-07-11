#!/usr/bin/env bash
# test_auto_worktree.sh - the /agent spawn auto-worktree path (x-9c4c).
#
# When spawn.sh launches a code-implementing payload (/target | /do | /fix) into
# a repo's MAIN checkout, it deterministically creates a conductor worktree and
# launches THERE (worker born isolated). These tests drive spawn.sh end-to-end
# against a real temp git repo with a stubbed `fno` (no daemon), and assert the
# three failure modes from the design:
#   1. a /think (non-code) payload is NOT worktree'd
#   2. a worktree-add error fails safe to repo root, never blocks the launch
#   3. a re-spawn of the same node reuses the path (no collision)
# plus: an already-isolated cwd (a linked worktree) is not re-isolated.
#
# Self-contained: real git + jq, stubbed fno. HOME is pinned to the temp dir so
# the conductor worktrees land under it and never touch the real ~/conductor.
# Run:  bash skills/agent/tests/test_auto_worktree.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPAWN="$HERE/../scripts/spawn.sh"
TMP="$(mktemp -d -t agents-auto-wt.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

PASS=0; FAIL=0
ok()  { if [[ "$2" == "$3" ]]; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); printf 'FAIL: %s (want %q got %q)\n' "$1" "$3" "$2"; fi; }
has() { if printf '%s' "$2" | grep -qF "$3"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); printf 'FAIL: %s (%q not in %q)\n' "$1" "$3" "$2"; fi; }
no()  { if printf '%s' "$2" | grep -qF "$3"; then FAIL=$((FAIL+1)); printf 'FAIL: %s (%q UNEXPECTEDLY in %q)\n' "$1" "$3" "$2"; else PASS=$((PASS+1)); fi; }

# --- stub fno: claim/agents probes pass, spawn emits a valid 8-hex receipt.
# `worktree ensure` (x-73ca) mirrors the real verb's contract (cli/src/fno/
# worktree_cli/cli.py): path on stdout + exit 0 on success, NOTHING on failure,
# so spawn.sh reads $wt empty and falls back to repo root. The mechanism itself
# (origin/main base, branch reuse) is tested in test_worktree_ensure.py; this
# hermetic stub only needs the happy path + the two fail-safe branches the
# caller's logic depends on (stray-dir refusal, idempotent reuse). ----------
STUBDIR="$TMP/bin"; mkdir -p "$STUBDIR"
cat > "$STUBDIR/fno" <<'STUB'
#!/usr/bin/env bash
case "$1 $2" in
  "agents spawn-guard")  printf '{"verdict":"dispatchable"}\n'; exit 0 ;;
  "agents list")         printf '{"agents":[]}\n'; exit 0 ;;
  "agents spawn"|"agents host") printf '{"short_id":"deadbeef"}\n'; exit 0 ;;
  "claim release")       exit 0 ;;
  "worktree ensure")
    shift 2  # drop "worktree ensure"; parse "--repo R --name N"
    repo=""; wtname=""
    while [[ $# -gt 0 ]]; do
      # `shift; shift` (not `shift 2`) so a value-less trailing flag can't
      # wedge the loop re-seeing the same flag -- malformed input falls through
      # to the git-C check below and fail-safes empty, like the real verb.
      case "$1" in
        --repo) repo="${2:-}"; shift; shift ;;
        --name) wtname="${2:-}"; shift; shift ;;
        *) shift ;;
      esac
    done
    top="$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null)" || exit 1
    # main-checkout-only gate: a linked worktree (git-dir != git-common-dir) has
    # no business nesting another -> refuse (test 4's already-isolated cwd).
    gdir="$(git -C "$top" rev-parse --path-format=absolute --git-dir 2>/dev/null)"
    common="$(git -C "$top" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)"
    [[ -n "$gdir" && "$gdir" == "$common" ]] || exit 1
    wt="$HOME/conductor/workspaces/$(basename "$top")/$wtname"
    if [[ -d "$wt" ]]; then
      # reuse our own worktree; never clobber a stray dir (test 5's decoy).
      git -C "$wt" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
        && { printf '%s\n' "$wt"; exit 0; } || exit 1
    fi
    mkdir -p "$(dirname "$wt")"
    git -C "$top" worktree add "$wt" -b "feature/$wtname" >/dev/null 2>&1 || exit 1
    printf '%s\n' "$wt"; exit 0 ;;
  *)                     exit 0 ;;
esac
STUB
chmod +x "$STUBDIR/fno"

# --- a real main checkout to dispatch into ------------------------------------
REPO="$TMP/myrepo"
git init -q "$REPO"
git -C "$REPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
# spawn.sh reuses <repo>/scripts/setup/setup-worktree.sh if present; absent here,
# so the "footnote-ecosystem only" link step is correctly skipped.

run_spawn() { # <msg> [extra args...]: HOME-pinned, stubbed-fno spawn.sh run
  local msg="$1"; shift
  # claude /target passthrough = payload_mode passthrough (msg leads with `/`).
  HOME="$TMP" PATH="$STUBDIR:$PATH" \
    bash "$SPAWN" --name "spawn-x-9c4c-demo" --provider claude --payload-mode passthrough \
    --message "$msg" --node "x-9c4c" --cwd "$REPO" "$@" 2>"$TMP/err"
}

# 1. code payload into a MAIN checkout -> worktree created, receipt carries cwd.
out="$(run_spawn "/target no-merge x-9c4c")"; rc=$?
err="$(cat "$TMP/err")"
ok   "code-payload exit 0" "$rc" "0"
has  "code-payload launched" "$out" "result=launched"
# cwd value is double-quoted in the receipt (a path with spaces must not split fields).
has  "code-payload cwd in receipt" "$out" "cwd=\"$TMP/conductor/workspaces/myrepo/spawn-x-9c4c-demo\""
has  "code-payload worktree note" "$err" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-x-9c4c-demo"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-x-9c4c-demo" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: worktree dir not created"; }
# branch must be the fresh feature branch, not the protected default.
br="$(git -C "$TMP/conductor/workspaces/myrepo/spawn-x-9c4c-demo" branch --show-current)"
ok   "code-payload on feature branch" "$br" "feature/spawn-x-9c4c-demo"

# 2. re-spawn of the same node -> reuse (ensure is idempotent), no second
#    worktree. spawn.sh emits the same `auto-worktree: <path>` note either way;
#    the reuse guarantee is structural (one worktree, not two).
out2="$(run_spawn "/target no-merge x-9c4c")"; err2="$(cat "$TMP/err")"
has  "re-spawn note" "$err2" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-x-9c4c-demo"
cnt="$(git -C "$REPO" worktree list | grep -c "spawn-x-9c4c-demo")"
ok   "re-spawn single worktree" "$cnt" "1"

# 3. /think (non-code passthrough) payload -> NO worktree, launches in repo root.
out3="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-think-demo" \
  --provider claude --payload-mode passthrough --message "/think born-with-why for x-9c4c" \
  --node "x-f7c9" --cwd "$REPO" 2>"$TMP/err3")"
err3="$(cat "$TMP/err3")"
has  "think launched" "$out3" "result=launched"
no   "think no worktree note" "$err3" "auto-worktree:"
no   "think no cwd field" "$out3" "cwd="
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-think-demo" ]] && { FAIL=$((FAIL+1)); echo "FAIL: /think got a worktree"; } || PASS=$((PASS+1))

# 4. already a linked worktree -> not re-isolated (launch in place, no nesting).
WT="$TMP/conductor/workspaces/myrepo/spawn-x-9c4c-demo"  # the worktree from test 1
out4="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-nested-demo" \
  --provider claude --payload-mode passthrough --message "/do task 1.1" --node "x-other" \
  --cwd "$WT" 2>"$TMP/err4")"
err4="$(cat "$TMP/err4")"
has  "linked-wt launched" "$out4" "result=launched"
no   "linked-wt not re-isolated" "$err4" "auto-worktree:"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-nested-demo" ]] && { FAIL=$((FAIL+1)); echo "FAIL: linked worktree got nested"; } || PASS=$((PASS+1))

# 5. fail-safe: worktree-add blocked (path pre-occupied by a non-worktree dir)
#    -> launch still succeeds in repo root, never blocked (failure mode 2).
mkdir -p "$TMP/conductor/workspaces/myrepo/spawn-blocked-demo/decoy"
out5="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-blocked-demo" \
  --provider claude --payload-mode passthrough --message "/fix the bug" --node "x-blk" \
  --cwd "$REPO" 2>"$TMP/err5")"; rc5=$?
err5="$(cat "$TMP/err5")"
ok   "fail-safe exit 0" "$rc5" "0"
has  "fail-safe still launched" "$out5" "result=launched"
# ensure refuses the stray dir -> empty stdout -> spawn.sh stays in repo root and
# emits NO worktree note (isolation is best-effort; the launch is never blocked).
no   "fail-safe no worktree note" "$err5" "auto-worktree:"
no   "fail-safe no cwd field" "$out5" "cwd="

# 6. --cwd a SUBDIR of a main checkout -> still detected as a main checkout and
#    worktree'd (the git-common-dir is returned relative to the subdir; the cd+
#    pwd -P canonicalization must still resolve it to the repo root).
mkdir -p "$REPO/src/deep"
out6="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-subdir-demo" \
  --provider claude --message "/target x-sub" --node "x-sub" \
  --cwd "$REPO/src/deep" 2>"$TMP/err6")"
err6="$(cat "$TMP/err6")"
has  "subdir worktree'd" "$err6" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-subdir-demo"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-subdir-demo" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: subdir cwd not worktree'd"; }

# 7. codex/gemini BUILD payload reaches spawn.sh as a PROSE brief (no /target
#    prefix) but payload_mode=build -> still a code-writing worker, so it MUST be
#    isolated. A message-prefix-only check would miss this (the Codex P1 fix), and
#    these workers have no location gate to fail safe on.
out7="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-codex-build" \
  --provider codex --payload-mode build --node "x-cdx" \
  --message "Implement backlog node x-cdx following AGENTS.md. Commit and open a pull request for review; do not merge it." \
  --cwd "$REPO" 2>"$TMP/err7")"
err7="$(cat "$TMP/err7")"
has  "codex-build worktree'd" "$err7" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-codex-build"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-codex-build" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: codex build prose payload not worktree'd"; }

# 8. ask payload (one-shot question, any provider) -> NOT code-writing, no worktree.
out8="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-ask-demo" \
  --provider codex --payload-mode ask --node "x-ask" \
  --message "what does the dispatch guard do?" --cwd "$REPO" 2>"$TMP/err8")"
err8="$(cat "$TMP/err8")"
no   "ask no worktree note" "$err8" "auto-worktree:"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-ask-demo" ]] && { FAIL=$((FAIL+1)); echo "FAIL: ask payload got a worktree"; } || PASS=$((PASS+1))

# 9. prose handoff continues an existing document; it is not a fresh feature
#    build and must stay in the caller-selected cwd for relative-path fidelity.
out9="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "handoff-doc-demo" \
  --provider codex --payload-mode handoff --message "Read docs/handoff.md and continue." \
  --cwd "$REPO" 2>"$TMP/err9")"
err9="$(cat "$TMP/err9")"
has "handoff launched" "$out9" "result=launched"
no  "handoff no worktree note" "$err9" "auto-worktree:"
[[ -d "$TMP/conductor/workspaces/myrepo/handoff-doc-demo" ]] && { FAIL=$((FAIL+1)); echo "FAIL: handoff payload got a worktree"; } || PASS=$((PASS+1))

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]

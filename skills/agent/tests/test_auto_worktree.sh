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
  "agents spawn"|"agents host")
    # Record the arg vector so a test can assert the --cwd the worker launches
    # at (the whole point of the delegation path: repo root, not a worktree).
    printf '%s\n' "$*" >> "${SPAWN_ARGS_LOG:-/dev/null}"
    printf '{"short_id":"deadbeef"}\n'; exit 0 ;;
  "claim release")       exit 0 ;;
  "worktree ensure")
    shift 2  # drop "worktree ensure"; parse "--repo R --name N [--harness H]"
    # Record the full arg vector so a test can assert --harness forwarding.
    printf '%s\n' "$*" >> "${ENSURE_ARGS_LOG:-/dev/null}"
    repo=""; wtname=""; harness=""
    while [[ $# -gt 0 ]]; do
      # `shift; shift` (not `shift 2`) so a value-less trailing flag can't
      # wedge the loop re-seeing the same flag -- malformed input falls through
      # to the git-C check below and fail-safes empty, like the real verb.
      case "$1" in
        --repo) repo="${2:-}"; shift; shift ;;
        --name) wtname="${2:-}"; shift; shift ;;
        --harness) harness="${2:-}"; shift; shift ;;
        *) shift ;;
      esac
    done
    top="$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null)" || exit 1
    # main-checkout-only gate: a linked worktree (git-dir != git-common-dir) has
    # no business nesting another -> refuse (test 4's already-isolated cwd).
    gdir="$(git -C "$top" rev-parse --path-format=absolute --git-dir 2>/dev/null)"
    common="$(git -C "$top" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)"
    [[ -n "$gdir" && "$gdir" == "$common" ]] || exit 1
    # Simulated policy=never: a repo named "nevrepo" launches in place (repo root
    # on stdout, exit 0, NO worktree) -- mirrors the real verb's never receipt.
    if [[ "$(basename "$top")" == "nevrepo" ]]; then printf '%s\n' "$top"; exit 0; fi
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
# `git rev-parse --show-toplevel` resolves symlinks, and macOS's /var -> /private/var
# makes that differ from $REPO. Assertions on a launch cwd must use the physical form.
REPO_PHYS="$(cd "$REPO" && pwd -P)"
git -C "$REPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
# spawn.sh reuses <repo>/scripts/setup/setup-worktree.sh if present; absent here,
# so the "footnote-ecosystem only" link step is correctly skipped.

run_spawn() { # <msg> [extra args...]: HOME-pinned, stubbed-fno spawn.sh run
  local msg="$1"; shift
  # claude /fix passthrough = payload_mode passthrough (msg leads with `/`).
  # /fix (like /do) refuses on a protected branch rather than isolating itself,
  # so it is the code payload that still needs spawn-side pre-creation -- and so
  # exercises the worktree mechanics below. A claude /target DELEGATES instead
  # (tests 13-15); this helper deliberately does not use one.
  HOME="$TMP" PATH="$STUBDIR:$PATH" \
    bash "$SPAWN" --name "spawn-x-9c4c-demo" --provider claude --payload-mode passthrough \
    --message "$msg" --node "x-9c4c" --cwd "$REPO" "$@" 2>"$TMP/err"
}

# 1. code payload into a MAIN checkout -> worktree created, receipt carries cwd.
out="$(run_spawn "/fix no-merge x-9c4c")"; rc=$?
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
out2="$(run_spawn "/fix no-merge x-9c4c")"; err2="$(cat "$TMP/err")"
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
  --provider claude --payload-mode passthrough --message "/do x-sub" --node "x-sub" \
  --cwd "$REPO/src/deep" 2>"$TMP/err6")"
err6="$(cat "$TMP/err6")"
has  "subdir worktree'd" "$err6" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-subdir-demo"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-subdir-demo" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: subdir cwd not worktree'd"; }

# 7. A BUILD payload isolates regardless of its rendered message shape: worktree
#    isolation keys on payload_mode=build (a code-writing worker), not on a
#    /target prefix (the Codex P1 fix). A non-slash message still isolates, and
#    these workers have no location gate to fail safe on.
out7="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-codex-build" \
  --provider codex --payload-mode build --node "x-cdx" \
  --message "Implement backlog node x-cdx following AGENTS.md. Commit and open a pull request for review; do not merge it." \
  --cwd "$REPO" 2>"$TMP/err7")"
err7="$(cat "$TMP/err7")"
has  "codex-build worktree'd" "$err7" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-codex-build"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-codex-build" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: build payload not worktree'd"; }

# 8. seed payload (verbatim free-text pane, x-cbb0) -> NOT code-writing, no worktree.
out8="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-seed-demo" \
  --provider codex --payload-mode seed \
  --message "what does the dispatch guard do?" --cwd "$REPO" 2>"$TMP/err8")"
err8="$(cat "$TMP/err8")"
no   "seed no worktree note" "$err8" "auto-worktree:"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-seed-demo" ]] && { FAIL=$((FAIL+1)); echo "FAIL: seed payload got a worktree"; } || PASS=$((PASS+1))

# 9. prose handoff continues an existing document; it is not a fresh feature
#    build and must stay in the caller-selected cwd for relative-path fidelity.
out9="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "handoff-doc-demo" \
  --provider codex --payload-mode handoff --message "Read docs/handoff.md and continue." \
  --cwd "$REPO" 2>"$TMP/err9")"
err9="$(cat "$TMP/err9")"
has "handoff launched" "$out9" "result=launched"
no  "handoff no worktree note" "$err9" "auto-worktree:"
[[ -d "$TMP/conductor/workspaces/myrepo/handoff-doc-demo" ]] && { FAIL=$((FAIL+1)); echo "FAIL: handoff payload got a worktree"; } || PASS=$((PASS+1))

# 10. the resolved harness is forwarded to ensure (--harness claude for provider
#     claude), so ensure's policy gate can land a claude payload harness-native.
: > "$TMP/ensure-args"
out10="$(HOME="$TMP" PATH="$STUBDIR:$PATH" ENSURE_ARGS_LOG="$TMP/ensure-args" \
  bash "$SPAWN" --name "spawn-harness-demo" --provider claude --payload-mode passthrough \
  --message "/fix x-hn" --node "x-hn" --cwd "$REPO" 2>"$TMP/err10")"
# fragment omits the leading dashes so the grep-based `has` helper does not parse
# "--harness" as its own option; the recorded arg vector still proves forwarding.
has  "harness forwarded to ensure" "$(cat "$TMP/ensure-args")" "harness claude"

# 11. policy=never -> ensure returns the repo root -> spawn.sh launches in place,
#     emits the never note, and does NOT run setup-worktree.sh on the canonical
#     checkout (Locked Decision 4: no worktree-only side effect on path == root).
NEVREPO="$TMP/nevrepo"
git init -q "$NEVREPO"
git -C "$NEVREPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
mkdir -p "$NEVREPO/scripts/setup"
cat > "$NEVREPO/scripts/setup/setup-worktree.sh" <<'S'
#!/usr/bin/env bash
touch "${WORKTREE:-$PWD}/.setup-ran"
S
chmod +x "$NEVREPO/scripts/setup/setup-worktree.sh"
out11="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-never-demo" \
  --provider claude --payload-mode passthrough --message "/do x-nev" --node "x-nev" \
  --cwd "$NEVREPO" 2>"$TMP/err11")"
err11="$(cat "$TMP/err11")"
has  "never launched in place note" "$err11" "policy=never, launching in place"
no   "never no real-worktree note" "$err11" "auto-worktree: $TMP/conductor"
[[ -f "$NEVREPO/.setup-ran" ]] && { FAIL=$((FAIL+1)); echo "FAIL: setup-worktree.sh ran on the canonical never checkout"; } || PASS=$((PASS+1))
# no auto-worktree was created, so the receipt advertises no worktree cwd field
# (the worker still launches in the repo root via --cwd, same as a /think payload).
no   "never no auto-worktree cwd field" "$out11" "cwd="

# 12. an explicit /target passthrough on opencode/codex is per-harness namespaced
#     (`/fno:target` / `$fno:target`), yet is still a code-writing verb and MUST
#     isolate - a prefix-only /target check would let these no-location-gate
#     workers edit the main checkout (gemini review, PR #444).
out12="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-oc-pass" \
  --provider opencode --payload-mode passthrough \
  --message "/fno:target ship the thing" --cwd "$REPO" 2>"$TMP/err12")"
err12="$(cat "$TMP/err12")"
has  "opencode /fno:target passthrough worktree'd" "$err12" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-oc-pass"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-oc-pass" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: opencode /fno:target passthrough not worktree'd"; }
out13="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-cdx-pass" \
  --provider codex --payload-mode passthrough \
  --message '$fno:target ship the thing' --cwd "$REPO" 2>"$TMP/err13")"
err13="$(cat "$TMP/err13")"
has  "codex \$fno:target passthrough worktree'd" "$err13" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-cdx-pass"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-cdx-pass" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: codex \$fno:target passthrough not worktree'd"; }
# a NON-code namespaced passthrough (/fno:think writes a design doc) stays in root
out14="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-oc-think" \
  --provider opencode --payload-mode passthrough \
  --message "/fno:think about the design" --cwd "$REPO" 2>"$TMP/err14")"
err14="$(cat "$TMP/err14")"
no   "opencode /fno:think no worktree note" "$err14" "auto-worktree:"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-oc-think" ]] && { FAIL=$((FAIL+1)); echo "FAIL: /fno:think got a worktree"; } || PASS=$((PASS+1))

# --- delegation: a claude /target isolates itself, so spawn.sh must NOT (x-6c22)
# The session's PROJECT is fixed at launch cwd with no rename hook, so launching
# in a pre-created worktree mints a throwaway ~/.claude/projects/ dir per spawn.
# The worker's own cold-start (`fno target start` -> EnterWorktree) creates the
# worktree instead, keeping the transcript in the repo's canonical project dir.

# 13. claude /target passthrough -> no worktree here, launched at the repo ROOT.
: > "$TMP/spawn-args"
out15="$(HOME="$TMP" PATH="$STUBDIR:$PATH" SPAWN_ARGS_LOG="$TMP/spawn-args" \
  bash "$SPAWN" --name "spawn-deleg-pass" --provider claude --payload-mode passthrough \
  --message "/target x-dlg" --node "x-dlg" --cwd "$REPO" 2>"$TMP/err15")"
err15="$(cat "$TMP/err15")"
has  "delegated launched" "$out15" "result=launched"
has  "delegated note names repo root" "$err15" "delegated to the worker cold-start (launching at $REPO_PHYS)"
no   "delegated no cwd receipt field" "$out15" "cwd="
# fragment omits the leading dashes so the grep-based `has` helper does not parse
# "--cwd" as its own option (same trick as the harness-forwarding test above).
has  "delegated launches at repo root" "$(cat "$TMP/spawn-args")" "cwd $REPO_PHYS "
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-deleg-pass" ]] && { FAIL=$((FAIL+1)); echo "FAIL: claude /target got a pre-created worktree"; } || PASS=$((PASS+1))

# 14. a node BUILD dispatch renders `/target <id>` on claude -> also delegates.
: > "$TMP/spawn-args"
mkdir -p "$REPO/src/deep"
out16="$(HOME="$TMP" PATH="$STUBDIR:$PATH" SPAWN_ARGS_LOG="$TMP/spawn-args" \
  bash "$SPAWN" --name "spawn-deleg-build" --provider claude --payload-mode build \
  --message "/target no-merge x-bld" --node "x-bld" --cwd "$REPO/src/deep" 2>"$TMP/err16")"
err16="$(cat "$TMP/err16")"
# from a SUBDIR: launching there would slug its own project dir, so the repo root
# (not the caller cwd) is what gets passed through.
has  "build delegated at repo root" "$(cat "$TMP/spawn-args")" "cwd $REPO_PHYS "
no   "build delegated not at subdir" "$(cat "$TMP/spawn-args")" "src/deep"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-deleg-build" ]] && { FAIL=$((FAIL+1)); echo "FAIL: claude build got a pre-created worktree"; } || PASS=$((PASS+1))

# 15. a `worktree: never` project gets no worktree from the spawn path at all -
#     cold-start's policy resolution is authoritative and is never pre-empted.
out17="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-deleg-never" \
  --provider claude --payload-mode passthrough --message "/target x-nev2" --node "x-nev2" \
  --cwd "$NEVREPO" 2>"$TMP/err17")"
err17="$(cat "$TMP/err17")"
has  "never+delegated launched" "$out17" "result=launched"
no   "never+delegated no cwd field" "$out17" "cwd="
[[ -f "$NEVREPO/.setup-ran" ]] && { FAIL=$((FAIL+1)); echo "FAIL: setup-worktree.sh ran on a delegated never checkout"; } || PASS=$((PASS+1))

# 16. non-claude keeps pre-creation: a codex/opencode worker can run `fno target
#     start` but has no EnterWorktree tool to move its session into the result,
#     so removing pre-creation there would strand it on the main checkout.
out18="$(HOME="$TMP" PATH="$STUBDIR:$PATH" bash "$SPAWN" --name "spawn-oc-target" \
  --provider opencode --payload-mode build \
  --message "/fno:target x-oc" --node "x-oc" --cwd "$REPO" 2>"$TMP/err18")"
err18="$(cat "$TMP/err18")"
has  "opencode build still pre-created" "$err18" "auto-worktree: $TMP/conductor/workspaces/myrepo/spawn-oc-target"
no   "opencode build not delegated" "$err18" "delegated to the worker cold-start"
[[ -d "$TMP/conductor/workspaces/myrepo/spawn-oc-target" ]] && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: opencode build lost its worktree"; }

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]

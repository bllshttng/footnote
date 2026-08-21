#!/usr/bin/env bash
# test_emit_attestation.sh
#
# Unit tests for skills/review/scripts/emit-attestation.sh, focused on the
# `branch` field (x-e601): the events journal is shared across every worktree
# of a repo by design, so `branch` is what scopes an attestation to the PR it
# reviewed. Verifies the stored payload (via a stubbed event sink, never the
# receipt - the two are what drift apart when a field is added on one side)
# and the receipt line's branch field.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
EMITTER="$REPO_ROOT/skills/review/scripts/emit-attestation.sh"

[[ -f "$EMITTER" ]] || { echo "FAIL: emitter not found at $EMITTER" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t emit-attestation-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Stub the event sink and CAPTURE ITS ARGV: the assertion must read what was
# actually handed to `doctor event emit`, not what the receipt claims (same rule as
# tests/hooks/test_attest_model.sh).
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$@" > "%s/last-emit.txt"\nexit 0\n' \
  "$TMP" > "$TMP/fno-stub"
chmod +x "$TMP/fno-stub"

# A scratch repo: the emitter refuses to run outside git (it head-pins).
REPO="$TMP/repo"
git init -q -b feature/x-e601 "$REPO"
git -C "$REPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
HEAD_SHA="$(git -C "$REPO" rev-parse HEAD)"

emit() { # runs the emitter inside the scratch repo; prints nothing
  (cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
    FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass) >/dev/null 2>&1
}

stored() { # echoes one field of the last emitted payload: stored <jq-path>
  [[ -f "$TMP/last-emit.txt" ]] || { echo "<no-emit>"; return; }
  # argv is: doctor event emit -t <kind> -s <source> -d <json>; the payload is -d's value.
  tail -1 "$TMP/last-emit.txt" | jq -r "$1 // \"<missing>\""
}

# 1. On a branch: the payload records the branch, and the receipt names it.
rm -f "$TMP/last-emit.txt"
RECEIPT="$(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass 2>&1 >/dev/null)"; RECEIPT_RC=$?
[[ $RECEIPT_RC -eq 0 ]] || fail "emitter exited $RECEIPT_RC on a branch"
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] && pass "payload records branch=feature/x-e601" \
  || fail "payload branch: want feature/x-e601, got '$got'"
case "$RECEIPT" in
  *"branch=feature/x-e601"*) pass "receipt names the branch" ;;
  *) fail "receipt lacks branch=feature/x-e601: $RECEIPT" ;;
esac

# 2. Detached HEAD: the literal "HEAD" names no branch, and an empty branch
#    field is byte-identical to the pre-branch-field backlog - a live emit
#    would mint a fresh legacy member that no later carry can scope. So the
#    emitter refuses: nonzero exit, no event, and the reason names the shape.
git -C "$REPO" checkout -q --detach
rm -f "$TMP/last-emit.txt"
RECEIPT="$(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass 2>&1 >/dev/null)"
RECEIPT_RC=$?
[[ $RECEIPT_RC -ne 0 ]] && pass "detached HEAD refuses to emit" \
  || fail "detached HEAD emitted anyway (exit $RECEIPT_RC)"
[[ ! -f "$TMP/last-emit.txt" ]] && pass "detached HEAD writes no event" \
  || fail "detached HEAD wrote an event: $(stored '.branch')"
case "$RECEIPT" in
  *"detached HEAD names no PR branch"*) pass "refusal names the shape" ;;
  *) fail "refusal lacks the reason: $RECEIPT" ;;
esac

# 3. The pre-existing contract is intact: head_sha pins the repo HEAD and the
#    reviewer/verdict round-trip. A new field must not cost an old one.
git -C "$REPO" checkout -q feature/x-e601
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.head_sha')"
[[ "$got" == "$HEAD_SHA" ]] && pass "payload head_sha pins the repo HEAD" \
  || fail "payload head_sha: want ${HEAD_SHA:0:8}, got '${got:0:8}'"
got="$(stored '.reviewer')"
[[ "$got" == "code-review" ]] && pass "payload reviewer round-trips" \
  || fail "payload reviewer: want code-review, got '$got'"
got="$(stored '.verdict')"
[[ "$got" == "pass" ]] && pass "payload verdict round-trips" \
  || fail "payload verdict: want pass, got '$got'"

# 4. A reviewer worktree branch (git refuses two worktrees on one branch)
#    tracking the PR branch upstream: the payload records the UPSTREAM short
#    name - what GitHub reports as headRefName - not the local worktree name.
#    The local name would never equal the PR's branch, so the post-rebase
#    carry would die on the branch arm over a pass that belongs here.
git -C "$REPO" remote add origin "$TMP/origin.git"
git -C "$REPO" update-ref refs/remotes/origin/feature/x-e601 "$HEAD_SHA"
git -C "$REPO" checkout -q -b wt/r-1
git -C "$REPO" branch --set-upstream-to=origin/feature/x-e601 wt/r-1 >/dev/null 2>&1
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] \
  && pass "worktree branch records its upstream PR branch" \
  || fail "worktree branch: want feature/x-e601 (upstream), got '$got'"

# 5. An author worktree pre-push: the branch tracks origin/main (fno creates
#    worktrees off main and `push -u` only fires at PR create), so the
#    upstream names the BASE. The LOCAL name is the PR branch and must win,
#    or every pre-push emit mis-scopes to main and loses the carry.
git -C "$REPO" checkout -q feature/x-e601
git -C "$REPO" update-ref refs/remotes/origin/main "$HEAD_SHA"
git -C "$REPO" branch --set-upstream-to=origin/main feature/x-e601 >/dev/null 2>&1
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] \
  && pass "a main-tracking branch keeps its local PR-branch name" \
  || fail "main-tracking branch: want feature/x-e601 (local), got '$got'"

# 6. A develop-based repo (round 3, PR 917): origin/HEAD names develop, and
#    the author branch tracks the BASE origin/develop pre-push. A literal
#    `main` comparison recorded branch=develop here - scoping the author's
#    feature-branch attestation to a base branch no PR ever carries. The base
#    must resolve through refs/remotes/origin/HEAD, and the local name wins.
git -C "$REPO" update-ref refs/remotes/origin/develop "$HEAD_SHA"
git -C "$REPO" symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/develop
git -C "$REPO" branch --set-upstream-to=origin/develop feature/x-e601 >/dev/null 2>&1
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] \
  && pass "a develop-tracking branch on a develop-based repo keeps its local name" \
  || fail "develop-tracking branch: want feature/x-e601 (local), got '$got'"

# 7. Same develop-based repo, reviewer shape: the upstream names the PR branch
#    (develop is now the base), so the upstream short name must still win -
#    the base resolution must not swallow the reviewer lane it exists for.
git -C "$REPO" checkout -q -b wt/r-2
git -C "$REPO" branch --set-upstream-to=origin/feature/x-e601 wt/r-2 >/dev/null 2>&1
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] \
  && pass "develop-based repo still records a reviewer upstream PR branch" \
  || fail "develop-based reviewer worktree: want feature/x-e601 (upstream), got '$got'"

# 8. The shape cases 6 and 7 do NOT cover: an author worktree tracking a
#    branch that is NOT origin/HEAD. origin/HEAD names main, the worktree was
#    created off origin/develop, and the author has committed. The base-name
#    test alone passes here (develop != main) and recorded branch=develop -
#    the round-3 bug relocated, losing the branch arm for the real PR and
#    leaking scope into any PR whose headRefName is literally develop. The
#    commits-ahead count is what separates an author worktree from a reviewer
#    one, so the LOCAL name must win.
git -C "$REPO" checkout -q feature/x-e601
git -C "$REPO" symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main
git -C "$REPO" branch --set-upstream-to=origin/develop feature/x-e601 >/dev/null 2>&1
git -C "$REPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m "author work"
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] \
  && pass "an author worktree tracking a non-default branch keeps its local name" \
  || fail "non-default-tracking author worktree: want feature/x-e601 (local), got '$got'"

# 9. The same repo, reviewer shape, with the PR branch now AHEAD of where the
#    reviewer worktree was cut. The reviewer still carries no commits of its
#    own, so the upstream must still win - the commits-ahead conjunct must not
#    swallow the lane it was added beside.
git -C "$REPO" update-ref refs/remotes/origin/feature/x-e601 "$(git -C "$REPO" rev-parse HEAD)"
git -C "$REPO" checkout -q -b wt/r-3 origin/feature/x-e601
git -C "$REPO" branch --set-upstream-to=origin/feature/x-e601 wt/r-3 >/dev/null 2>&1
rm -f "$TMP/last-emit.txt"
emit
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] \
  && pass "a reviewer worktree with no commits of its own still records the PR branch" \
  || fail "reviewer worktree: want feature/x-e601 (upstream), got '$got'"

echo ""
echo "emit-attestation: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]

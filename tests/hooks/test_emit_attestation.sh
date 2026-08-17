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
# actually handed to `event emit`, not what the receipt claims (same rule as
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
  # argv is: event emit -t <kind> -s <source> -d <json>; the payload is -d's value.
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

# 2. Detached HEAD: the literal "HEAD" names no branch, so the field is empty
#    in the payload and the receipt says detached. An empty field is the
#    pre-branch-field shape, which consumers admit on exact head equality
#    only - recording the string "HEAD" would invent a branch nobody can match.
git -C "$REPO" checkout -q --detach
rm -f "$TMP/last-emit.txt"
RECEIPT="$(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass 2>&1 >/dev/null)"
got="$(stored '.branch')"
[[ "$got" == "" ]] && pass "detached HEAD stores an empty branch" \
  || fail "detached HEAD branch: want empty, got '$got'"
case "$RECEIPT" in
  *"branch=detached"*) pass "receipt says detached rather than naming HEAD" ;;
  *) fail "receipt lacks branch=detached: $RECEIPT" ;;
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

echo ""
echo "emit-attestation: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]

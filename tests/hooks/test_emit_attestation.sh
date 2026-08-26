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
# Discriminates on the verb, like tests/hooks/test_code_review_attest.sh's stub:
# the emitter shells `fno` for more than the event sink now (it clears the
# review hold once a verdict exists for the head), and a stub that captured
# EVERY call overwrote the payload under assertion with the last unrelated one.
printf '#!/usr/bin/env bash\nif [[ "$1" == "doctor" && "$2" == "event" && "$3" == "emit" ]]; then printf "%%s\\n" "$@" > "%s/last-emit.txt"; fi\nexit 0\n' \
  "$TMP" > "$TMP/fno-stub"
chmod +x "$TMP/fno-stub"

# A scratch repo: the emitter refuses to run outside git (it head-pins). The
# fixture carries a REAL code diff (base commit A on origin/main, feature
# commit B on the branch) because the emitter measures the diff under review
# and refuses a zero-line one - an empty-commit fixture would trip that
# refusal on every emit below.
REPO="$TMP/repo"
git init -q -b feature/x-e601 "$REPO"
git -C "$REPO" config user.email t@t.t
git -C "$REPO" config user.name t
echo one > "$REPO/a.txt"
git -C "$REPO" add a.txt
git -C "$REPO" commit -qm "base"
BASE_SHA="$(git -C "$REPO" rev-parse HEAD)"
git -C "$REPO" update-ref refs/remotes/origin/main "$BASE_SHA"
echo two >> "$REPO/a.txt"
echo body > "$REPO/b.txt"
git -C "$REPO" add a.txt b.txt
git -C "$REPO" commit -qm "feature"
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
git -C "$REPO" update-ref refs/remotes/origin/main "$BASE_SHA"
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
git -C "$REPO" update-ref refs/remotes/origin/develop "$BASE_SHA"
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

# 10. The review hold is released HERE, at the positive completion marker, so
#     the release and the proof of completion are one event. A release wired to
#     a separate "the tool returned" signal can fire while the review is still
#     writing fixes, which is the defect the hold exists to prevent.
ALL_CALLS="$TMP/all-calls.txt"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >> "%s"\nexit 0\n' \
  "$ALL_CALLS" > "$TMP/fno-all-stub"
chmod +x "$TMP/fno-all-stub"
: > "$ALL_CALLS"
(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-all-stub" bash "$EMITTER" code-review pass) >/dev/null 2>&1
grep -q "do pr review-hold release --branch feature/x-e601" "$ALL_CALLS" \
  && pass "a landed verdict releases the review hold" \
  || fail "no review-hold release after the attestation: $(cat "$ALL_CALLS")"

# No --holder. The hook names the HARNESS session; this script's session_id is
# grepped from target-state.md and falls back to "unknown". release_claim
# no-ops SILENTLY on a mismatch, so a holder-matched release wedged the lane
# for the full TTL under a receipt that said "released".
grep -q -- "--holder" "$ALL_CALLS" \
  && fail "the release passed a holder it cannot reconstruct" \
  || pass "the release names no holder"

# 12. A hold-created invocation id is copied onto both the reviewer observation
# and the completion attestation. This is the lifecycle join, not a receipt
# inferred from either command's stdout.
LINKED="$TMP/linked"
LINKED_REVIEW="$TMP/linked-review.json"
LINKED_ATTEST="$TMP/linked-attest.json"
cat > "$TMP/fno-linked-stub" <<STUB
#!/usr/bin/env bash
if [[ "\${1:-}" == "do" && "\${2:-}" == "pr" && "\${3:-}" == "review-hold" && "\${4:-}" == "metadata" ]]; then
  printf '%s\n' '{"metadata":{"invocation_id":"ri-linked-positive","verb":"/code-review","args_raw":"medium --comment","level":"medium","level_source":"explicit","flags":["--comment"]}}'
  exit 0
fi
if [[ "\${1:-}" == "doctor" && "\${2:-}" == "event" && "\${3:-}" == "emit" ]]; then
  type=""; data=""
  while [[ \$# -gt 0 ]]; do
    case "\$1" in
      -t) type="\$2"; shift 2 ;;
      -d) data="\$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  if [[ "\$type" == "review_invocation" ]]; then
    printf '%s\n' "\$data" > "$LINKED_REVIEW"
  elif [[ "\$type" == "review_attestation" ]]; then
    printf '%s\n' "\$data" > "$LINKED_ATTEST"
  fi
fi
exit 0
STUB
chmod +x "$TMP/fno-linked-stub"
rm -f "$LINKED_REVIEW" "$LINKED_ATTEST"
mkdir -p "$TMP/fno-home/review-invocations/ri-linked-positive.subagents"
touch "$TMP/fno-home/review-invocations/ri-linked-positive.subagents/subagent-1"
(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-linked-stub" FNO_HOME="$TMP/fno-home" \
  bash "$EMITTER" code-review pass shared inline report_findings) >/dev/null 2>&1
if [[ -s "$LINKED_REVIEW" && -s "$LINKED_ATTEST" ]]; then
  pass "lifecycle join emits reviewer and attestation markers"
else
  fail "lifecycle join did not emit both records"
fi
if jq -e '.invocation_id == "ri-linked-positive" and .execution_context == "inline" and .output_contract == "report_findings" and .subagent_count == 1' "$LINKED_REVIEW" >/dev/null 2>&1; then
  pass "reviewer observation carries id, trigger contract, and depth"
else
  fail "reviewer observation lost lifecycle fields: $(cat "$LINKED_REVIEW" 2>/dev/null)"
fi
if jq -e '.invocation_id == "ri-linked-positive" and .reviewer == "code-review"' "$LINKED_ATTEST" >/dev/null 2>&1; then
  pass "review_attestation carries the invocation id"
else
  fail "review_attestation lost invocation id: $(cat "$LINKED_ATTEST" 2>/dev/null)"
fi

# 11. The payload records the diff under review: base (merge-base), head, and
#     a line count greater than 0. This is the positive control for the
#     refusal below - a refusal alone cannot separate "zero lines measured"
#     from "the emitter never ran".
git -C "$REPO" checkout -q feature/x-e601
rm -f "$TMP/last-emit.txt"
emit
LIVE_SHA="$(git -C "$REPO" rev-parse HEAD)"
got="$(stored '.reviewed_base_sha')"
[[ "$got" == "$BASE_SHA" ]] && pass "payload records reviewed_base_sha" \
  || fail "reviewed_base_sha: want ${BASE_SHA:0:8}, got '${got:0:8}'"
got="$(stored '.reviewed_head_sha')"
[[ "$got" == "$LIVE_SHA" ]] && pass "payload records reviewed_head_sha" \
  || fail "reviewed_head_sha: want ${LIVE_SHA:0:8}, got '${got:0:8}'"
got="$(stored '.reviewed_line_count')"
[[ "$got" =~ ^[0-9]+$ && "$got" -gt 0 ]] && pass "payload records a positive reviewed_line_count ($got)" \
  || fail "reviewed_line_count: want a positive integer, got '$got'"
got="$(stored '.reviewed_file_count')"
[[ "$got" =~ ^[0-9]+$ && "$got" -gt 0 ]] && pass "payload records the changed-file count ($got)" \
  || fail "reviewed_file_count: want a positive integer, got '$got'"

# 12. An EMPTY diff refuses: a checkout sitting AT the base (HEAD equals the
#     merge-base, no changed files) has read nothing, and a clean review of
#     nothing must not become a pass. The refusal names base and head, and no
#     event is written. Lines never decide this - case 12b shows why.
git -C "$REPO" checkout -q -b zero/at-base origin/main
rm -f "$TMP/last-emit.txt"
RECEIPT="$(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass 2>&1 >/dev/null)"
RECEIPT_RC=$?
[[ $RECEIPT_RC -ne 0 ]] && pass "empty diff refuses to emit" \
  || fail "empty diff emitted anyway (exit $RECEIPT_RC)"
[[ ! -f "$TMP/last-emit.txt" ]] && pass "empty diff writes no event" \
  || fail "empty diff wrote an event"
case "$RECEIPT" in
  *"the diff under review is empty (no changed files"*)
    pass "refusal names the empty shape" ;;
  *) fail "refusal lacks the reason: $RECEIPT" ;;
esac
case "$RECEIPT" in
  *"$BASE_SHA"*|*"${BASE_SHA:0:8}"*)
    pass "refusal names the base sha" ;;
  *) fail "refusal lacks the base sha: $RECEIPT" ;;
esac

# 12b. A binary-only diff is a real review: numstat prints "-" for binary
#      files, so the LINE count is an honest 0 while a file genuinely changed.
#      The emit must attest with lines=0 files=1 - a lines-only refusal would
#      close the sanctioned producer path for every images/fonts PR.
git -C "$REPO" checkout -q -b binary/only origin/main
printf 'PNG\x00\x89binary-bytes-no-text\x00' > "$REPO/asset.bin"
git -C "$REPO" add asset.bin
git -C "$REPO" commit -qm "binary asset"
rm -f "$TMP/last-emit.txt"
emit_rc=0
(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass) >/dev/null 2>&1 || emit_rc=$?
[[ $emit_rc -eq 0 ]] && pass "binary-only diff attests" \
  || fail "binary-only diff refused (the producer path closed)"
[[ -f "$TMP/last-emit.txt" ]] && pass "binary-only diff writes the event" \
  || fail "binary-only diff wrote no event"
if [[ -f "$TMP/last-emit.txt" ]]; then
  got_lines="$(stored '.reviewed_line_count')"
  got_files="$(stored '.reviewed_file_count')"
  [[ "$got_lines" == "0" ]] && pass "binary-only records lines=0 honestly" \
    || fail "binary-only lines: want 0, got '$got_lines'"
  [[ "$got_files" == "1" ]] && pass "binary-only records files=1" \
    || fail "binary-only files: want 1, got '$got_files'"
fi

# 13. An unmeasurable diff also refuses: with no origin/<base> and no local
#     <base> to merge against, the event cannot say what was read, and an
#     attestation that cannot name its diff must not be minted.
git -C "$REPO" checkout -q feature/x-e601
git -C "$REPO" update-ref -d refs/remotes/origin/main
rm -f "$TMP/last-emit.txt"
RECEIPT="$(cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
  FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass 2>&1 >/dev/null)"
RECEIPT_RC=$?
[[ $RECEIPT_RC -ne 0 ]] && pass "unresolvable base refuses to emit" \
  || fail "unresolvable base emitted anyway (exit $RECEIPT_RC)"
[[ ! -f "$TMP/last-emit.txt" ]] && pass "unresolvable base writes no event" \
  || fail "unresolvable base wrote an event"

# 14-16. --findings-file (AC2-HP / AC2-ERR / AC2-EDGE). The classify leg runs
# the REAL classifier from the repo tree (PYTHONPATH, no fno binary), so these
# cases exercise producer-true classification through the emitter's transport:
# the merge into the event payload, the refusals, and the no-file byte shape.
git -C "$REPO" update-ref refs/remotes/origin/main "$BASE_SHA"
git -C "$REPO" checkout -q feature/x-e601

export CLASSIFY_PYTHONPATH="$REPO_ROOT/cli/src"
export CLASSIFY_PYTHON="$REPO_ROOT/cli/.venv/bin/python"
cat > "$TMP/classify-real.sh" <<'CREAL'
#!/usr/bin/env bash
f="$1"
PYTHONPATH="$CLASSIFY_PYTHONPATH" "$CLASSIFY_PYTHON" - "$f" <<'PY'
import json, sys
from fno.review.cli import build_emit_record, RecordBuildError
try:
    with open(sys.argv[1], encoding="utf-8") as fh:
        payload = json.load(fh)
except OSError as exc:
    print(f"classify: cannot read {sys.argv[1]}: {exc}", file=sys.stderr)
    raise SystemExit(2)
except ValueError as exc:
    print(f"classify: {sys.argv[1]} is not valid JSON: {exc}", file=sys.stderr)
    raise SystemExit(2)
try:
    print(json.dumps(build_emit_record(payload)))
except RecordBuildError as exc:
    print(f"classify: {sys.argv[1]}: {exc}", file=sys.stderr)
    raise SystemExit(2)
PY
CREAL
chmod +x "$TMP/classify-real.sh"

cat > "$TMP/fno-full" <<'FSTUB'
#!/usr/bin/env bash
stub_dir="$(cd "$(dirname "$0")" && pwd)"
if [[ "$1" == "doctor" && "$2" == "event" && "$3" == "emit" ]]; then
  printf '%s\n' "$@" > "$stub_dir/last-emit.txt"
  exit 0
fi
if [[ "$1" == "do" && "$2" == "review" && "$3" == "classify" ]]; then
  f=""; shift 3
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --findings-file) f="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  bash "$stub_dir/classify-real.sh" "$f"
  exit $?
fi
exit 0
FSTUB
chmod +x "$TMP/fno-full"

emit_ff() { # like emit(), through the dual stub; $1 = extra args string
  (cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
    FNO="$TMP/fno-full" bash "$EMITTER" code-review pass shared $1) >/dev/null 2>&1
}

echo "== --findings-file (AC2-HP) =="
F14="$TMP/findings-hp.json"
cat > "$F14" <<'J'
[
  {"category": "correctness", "file": "a.py", "line": 3, "summary": "off-by-one", "failure_scenario": "wrong total"},
  {"category": "typo", "file": "b.py", "line": 4, "summary": "teh", "failure_scenario": "reader stumble"},
  {"category": "nit", "file": "c.py", "line": 5, "summary": "name", "failure_scenario": "none"}
]
J
rm -f "$TMP/last-emit.txt"
emit_ff "--findings-file $F14"
[[ -f "$TMP/last-emit.txt" ]] && pass "findings file emits" \
  || fail "findings file emitted nothing"
got="$(stored '.findings_blocking')"
[[ "$got" == "1" ]] && pass "payload carries findings_blocking=1" \
  || fail "findings_blocking: want 1, got '$got'"
got="$(stored '.findings_nonblocking')"
[[ "$got" == "2" ]] && pass "payload carries findings_nonblocking=2" \
  || fail "findings_nonblocking: want 2, got '$got'"
got="$(stored '.findings | length')"
[[ "$got" == "3" ]] && pass "payload carries three finding primitives" \
  || fail "findings length: want 3, got '$got'"
got="$(stored '.findings[0].finding_key')"
[[ "$got" == "a.py:3:correctness" ]] && pass "primitive carries finding_key" \
  || fail "finding_key: want a.py:3:correctness, got '$got'"
got="$(stored '.reviewed_base_sha')"
[[ "$got" == "$BASE_SHA" ]] && pass "reviewed_base_sha unchanged by the record" \
  || fail "reviewed_base_sha changed: '$got'"
got="$(stored '.branch')"
[[ "$got" == "feature/x-e601" ]] && pass "branch unchanged by the record" \
  || fail "branch changed: '$got'"
got="$(stored '.reviewer_context')"
[[ "$got" == "shared" ]] && pass "reviewer_context unchanged by the record" \
  || fail "reviewer_context changed: '$got'"

echo "== --findings-file refusals (AC2-ERR) =="
for case in missing invalid notarray; do
  case "$case" in
    missing) F15="$TMP/findings-missing.json" ;;
    invalid) F15="$TMP/findings-invalid.json"; printf 'not json {' > "$F15" ;;
    notarray) F15="$TMP/findings-notarray.json"; printf '{"oops": true}' > "$F15" ;;
  esac
  rm -f "$TMP/last-emit.txt"
  emit_rc=0
  (cd "$REPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
    FNO="$TMP/fno-full" bash "$EMITTER" code-review pass shared --findings-file "$F15") >/dev/null 2>"$TMP/err.txt" || emit_rc=$?
  [[ $emit_rc -ne 0 ]] && pass "$case findings file refuses (exit $emit_rc)" \
    || fail "$case findings file emitted anyway"
  [[ ! -f "$TMP/last-emit.txt" ]] && pass "$case findings file writes no event" \
    || fail "$case findings file wrote an event"
  grep -q "$F15" "$TMP/err.txt" && pass "$case refusal names the file" \
    || fail "$case refusal lacks the file name: $(cat "$TMP/err.txt")"
done

echo "== no --findings-file stays byte-identical (AC2-EDGE) =="
rm -f "$TMP/last-emit.txt"
emit_ff ""
[[ -f "$TMP/last-emit.txt" ]] && pass "no-file path still emits" \
  || fail "no-file path stopped emitting"
for key in findings_blocking findings_nonblocking findings review_round dispositions; do
  got="$(stored ".$key")"
  [[ "$got" == "<missing>" ]] && pass "no-file payload omits $key" \
    || fail "no-file payload carries $key='$got'"
done

echo ""
echo "emit-attestation: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]

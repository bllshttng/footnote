#!/usr/bin/env bash
# test_target_self_handoff.sh - durable resume receipt revalidation (x-c3a2)
#
# Integration coverage for the receipt writer + read-only revalidate gate,
# exercised through the `fno resume receipt` CLI in a controlled temp repo.
# The unit test (cli/tests/unit/test_resume_receipt.py) covers the pure logic;
# this proves the wiring (live git + claim + journal gathering) and every
# fail-closed case the successor-facing contract names:
#
#   S1  write + show + validate ok
#   S2  immutability: same-identity rewrite refused (already_exists)
#   S3  stale_head: worker committed after the receipt (the kill-after-commit-
#       before-journal case) -> a successor fails closed rather than replay
#   S4  dead_worktree
#   S5  foreign_claim: node held by another session
#   S6  duplicate_generation: a delegated event already minted this generation
#   S7  superseded_by_later_event: a later terminal postdates the receipt
#   S8  malformed_receipt: present-but-corrupt file fails closed
#   S9  idempotency keys round-trip: a resumed worker sees recorded effect keys
#
# Self-contained: temp git repo + temp claims root. No real node/PR touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

PASS=0; FAIL=0; SKIP_COUNT=0
log()  { printf '[self-handoff] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[self-handoff] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[self-handoff] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[self-handoff] SKIP: %s\n' "$*"; }

command -v git >/dev/null 2>&1 || { fail "git required"; exit 1; }

PYTHON_CLI=""
CLI_VENV="${REPO_ROOT}/cli/.venv/bin/python"
if [[ -x "$CLI_VENV" ]] && PYTHONPATH="${REPO_ROOT}/cli/src" "$CLI_VENV" -c "import fno" 2>/dev/null; then
  PYTHON_CLI="$CLI_VENV"
else
  # Fall back to uv-managed env if present (dev worktree without a committed venv).
  if command -v uv >/dev/null 2>&1 && [[ -f "${REPO_ROOT}/cli/pyproject.toml" ]]; then
    PYTHON_CLI="uv run --project ${REPO_ROOT}/cli python"
  fi
fi
if [[ -z "$PYTHON_CLI" ]]; then
  skip "no fno python available (need cli/.venv or uv); nothing to test"
  printf '[self-handoff] RESULTS: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
  exit 0
fi

FNO() { (cd "$TMP" && PYTHONPATH="${REPO_ROOT}/cli/src" $PYTHON_CLI -m fno.cli "$@"); }
node_of() { python3 -c "import sys,json; print(json.load(sys.stdin)['identity']['node'])"; }
field() { python3 -c "import sys,json; d=json.load(sys.stdin); v=d.get('$1',''); print(v if not isinstance(v,(dict,list)) else json.dumps(v))"; }

# Runs validate in the CURRENT shell (not a command substitution) so VAL_RC
# propagates. Sets globals VAL (stdout json) and VAL_RC (exit code).
run_validate() {
  VAL="$(FNO_CLAIMS_ROOT="$CLAIMS_HOME" FNO resume receipt validate \
      --node "$NODE" --session s1 --claims-root "$CLAIMS_HOME" "$@" 2>/dev/null)"; VAL_RC=$?
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
CLAIMS_HOME="$TMP/claims-home"
mkdir -p "$CLAIMS_HOME/.fno/claims"

# Minimal git repo so resolve_repo_root + git rev-parse work inside $TMP.
git init -q "$TMP"
git -C "$TMP" config user.email t@t.tt
git -C "$TMP" config user.name test
git -C "$TMP" checkout -q -b feature/x-test 2>/dev/null || git -C "$TMP" switch -q -b feature/x-test
mkdir -p "$TMP/.fno/artifacts/handoff"
touch "$TMP/.fno/events.jsonl"
git -C "$TMP" add -A
git -C "$TMP" commit -q -m init >/dev/null
HEAD1="$(git -C "$TMP" rev-parse HEAD)"

NODE="x-test"
write_receipt() {
  # $1=head override (default HEAD1), $2=extra args
  local head="${1:-$HEAD1}"
  local extra="${2:-}"
  FNO_CLAIMS_ROOT="$CLAIMS_HOME" FNO resume receipt write \
    --node "$NODE" --session s1 --phase do --generation 2 \
    --repo testrepo --worktree "$TMP" --branch feature/x-test \
    --head "$head" --next-verb "/fno:execute waves" --next-target "$NODE" \
    --idempotency-keys "pr_create:${HEAD1:0:7}" $extra >/dev/null
}

# ── S1: write + show + validate ok ───────────────────────────────────────────
write_receipt
SHOW_NODE="$(FNO resume receipt show --node "$NODE" 2>/dev/null | node_of)"
[[ "$SHOW_NODE" == "$NODE" ]] && pass "S1a: show loads receipt for $NODE" || fail "S1a: show returned '$SHOW_NODE'"
run_validate
if [[ "$VAL_RC" -eq 0 ]] && printf '%s' "$VAL" | field ok | grep -q True; then
  pass "S1b: validate ok (head matches, worktree live, claim free)"
else
  fail "S1b: validate not ok (rc=$VAL_RC): $VAL"
fi

# ── S2: immutability (same identity rewrite refused) ─────────────────────────
W2_RC=0
write_receipt 2>/dev/null || W2_RC=$?
if [[ "$W2_RC" -ne 0 ]]; then
  pass "S2: same-identity rewrite refused (rc=$W2_RC)"
else
  fail "S2: rewrite should have been refused"
fi

# ── S3: stale_head (worker committed after receipt -> successor fails closed) ─
git -C "$TMP" commit -q --allow-empty -m advance >/dev/null
run_validate
REASON="$(printf '%s' "$VAL" | field reason)"
if [[ "$VAL_RC" -ne 0 && "$REASON" == "stale_head" ]]; then
  pass "S3: stale_head (successor fails closed, no replay)"
else
  fail "S3: expected stale_head, got rc=$VAL_RC reason='$REASON'"
fi
# Reset HEAD so later scenarios validate against the receipt's head again.
git -C "$TMP" reset -q --hard "$HEAD1" >/dev/null

# ── S4: dead_worktree ────────────────────────────────────────────────────────
run_validate --worktree "$TMP/nope"
REASON="$(printf '%s' "$VAL" | field reason)"
if [[ "$VAL_RC" -ne 0 && "$REASON" == "dead_worktree" ]]; then
  pass "S4: dead_worktree"
else
  fail "S4: expected dead_worktree, got rc=$VAL_RC reason='$REASON'"
fi

# ── S5: foreign_claim ────────────────────────────────────────────────────────
FNO_CLAIMS_ROOT="$CLAIMS_HOME" FNO agents claim acquire "node:$NODE" --holder other-session >/dev/null 2>&1
run_validate
REASON="$(printf '%s' "$VAL" | field reason)"
if [[ "$VAL_RC" -ne 0 && "$REASON" == "foreign_claim" ]]; then
  pass "S5: foreign_claim"
else
  fail "S5: expected foreign_claim, got rc=$VAL_RC reason='$REASON'"
fi
FNO_CLAIMS_ROOT="$CLAIMS_HOME" FNO agents claim release "node:$NODE" --holder other-session >/dev/null 2>&1 || true

# ── S6: duplicate_generation (delegated event from a foreign session) ────────
printf '{"type":"delegated","ts":"2026-07-26T02:30:00Z","data":{"node_id":"%s","harness":"claude","generation":2,"from_session":"other"}}\n' "$NODE" >> "$TMP/.fno/events.jsonl"
run_validate
REASON="$(printf '%s' "$VAL" | field reason)"
if [[ "$VAL_RC" -ne 0 && "$REASON" == "duplicate_generation" ]]; then
  pass "S6: duplicate_generation"
else
  fail "S6: expected duplicate_generation, got rc=$VAL_RC reason='$REASON'"
fi
: > "$TMP/.fno/events.jsonl"  # clear for next scenario

# ── S7: superseded_by_later_event (terminal after the receipt write) ─────────
# The receipt's written_at is real now; a far-future ts is unambiguously later.
printf '{"type":"termination","ts":"2031-12-31T23:59:59Z","data":{"node_id":"%s"}}\n' "$NODE" >> "$TMP/.fno/events.jsonl"
run_validate
REASON="$(printf '%s' "$VAL" | field reason)"
if [[ "$VAL_RC" -ne 0 && "$REASON" == superseded_by_later_event* ]]; then
  pass "S7: superseded_by_later_event"
else
  fail "S7: expected superseded_by_later_event*, got rc=$VAL_RC reason='$REASON'"
fi
: > "$TMP/.fno/events.jsonl"

# ── S8: malformed_receipt fails closed ───────────────────────────────────────
RECEIPT_FILE="$TMP/.fno/artifacts/handoff/receipt-${NODE}-do-g2-${HEAD1:0:12}.json"
if [[ -f "$RECEIPT_FILE" ]]; then
  printf '{corrupt' > "$RECEIPT_FILE"
  run_validate
  REASON="$(printf '%s' "$VAL" | field reason)"
  if [[ "$VAL_RC" -ne 0 && "$REASON" == "malformed_receipt" ]]; then
    pass "S8: malformed_receipt fails closed"
  else
    fail "S8: expected malformed_receipt, got rc=$VAL_RC reason='$REASON'"
  fi
else
  fail "S8: receipt file not found at $RECEIPT_FILE"
fi

# ── S9: idempotency keys round-trip (a resumed worker sees recorded keys) ────
# Re-write a fresh receipt on a new head so S8's corruption is replaced, and
# assert the validate verdict carries the recorded pr_create key.
git -C "$TMP" commit -q --allow-empty -m s9 >/dev/null
HEAD9="$(git -C "$TMP" rev-parse HEAD)"
FNO_CLAIMS_ROOT="$CLAIMS_HOME" FNO resume receipt write \
  --node "$NODE" --session s1 --phase review --generation 3 \
  --repo testrepo --worktree "$TMP" --branch feature/x-test \
  --head "$HEAD9" --next-verb "/fno:pr create" --next-target "$NODE" \
  --idempotency-keys "$(printf 'pr_create:%s\nmerge:%s' "${HEAD9:0:7}" "${HEAD9:0:7}")" >/dev/null
run_validate
KEYS="$(printf '%s' "$VAL" | python3 -c "import sys,json; d=json.load(sys.stdin); print(','.join(d.get('idempotency_keys',[])))")"
if printf '%s' "$KEYS" | grep -q "pr_create:${HEAD9:0:7}" && printf '%s' "$KEYS" | grep -q "merge:${HEAD9:0:7}"; then
  pass "S9: idempotency keys round-trip ($KEYS)"
else
  fail "S9: idempotency keys missing (got '$KEYS')"
fi

# ── S10: corrupted claim fails closed (not collapsed to free) ────────────────
# A garbage lockfile cannot confirm ownership; validate must fail closed with
# corrupted_claim rather than treat it as a free claim and grant ok.
CLAIM_FILE="$CLAIMS_HOME/.fno/claims/node%3A${NODE}.lock"
mkdir -p "$(dirname "$CLAIM_FILE")"
printf 'not a valid claim lockfile' > "$CLAIM_FILE"
run_validate
REASON="$(printf '%s' "$VAL" | field reason)"
if [[ "$VAL_RC" -ne 0 && "$REASON" == "corrupted_claim" ]]; then
  pass "S10: corrupted_claim fails closed"
else
  fail "S10: expected corrupted_claim, got rc=$VAL_RC reason='$REASON'"
fi
rm -f "$CLAIM_FILE"

printf '[self-handoff] RESULTS: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
[[ "$FAIL" -eq 0 ]]

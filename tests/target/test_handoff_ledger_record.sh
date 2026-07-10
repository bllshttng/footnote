#!/usr/bin/env bash
# AC7-EDGE (control-plane step 6, ab-f8e5f214): a self-handoff (delegating)
# session writes its OWN ledger session-record at the Step 8 commit point, via
# `fno-agents finalize --reason delegated` against the ARCHIVED manifest, BEFORE
# the stop-hook shim (which exits early on the now-missing manifest) would run.
#
# The completion side-effects (stamp/graduate/handoff) stay the successor's job,
# so the delegating session's finalize must use reason=delegated (a non-ship
# reason -> ledger row only).
#
# Strategy: drive the real skills/target/scripts/handoff.sh end-to-end in a
# sandbox with stubbed `fno` (claim/agents/event) and a stub `fno-agents` that
# records its `finalize` invocation. Then assert handoff committed the
# delegation AND invoked finalize with --reason delegated --state <archived>.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HANDOFF="${REPO_ROOT}/skills/target/scripts/handoff.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[handoff-ledger] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[handoff-ledger] FAIL: %s\n' "$*" >&2; }

[[ -f "$HANDOFF" ]] || { fail "handoff.sh not found at $HANDOFF"; exit 1; }
command -v jq >/dev/null 2>&1 || { printf '[handoff-ledger] SKIP: jq not on PATH\n' >&2; exit 77; }

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR:-/nonexistent}"' EXIT
HOME_DIR="${TMP_DIR}/home"; mkdir -p "$HOME_DIR"
PROJ="${TMP_DIR}/proj"
FNO_DIR="${PROJ}/.fno"
mkdir -p "$FNO_DIR"
BIN_DIR="${TMP_DIR}/bin"; mkdir -p "$BIN_DIR"

NODE_ID="ab-deadbeef"
SID="hsess-001"

# Plan file (status ready) so the precondition passes.
PLAN="${PROJ}/plan.md"
cat > "$PLAN" <<'PLAN'
---
status: ready
---
# plan
PLAN

# Manifest: session_id/plan_path/claude_transcript_id in frontmatter,
# graph_node_id + claim fields in the body (matching the real layout).
cat > "${FNO_DIR}/target-state.md" <<MAN
---
session_id: ${SID}
created_at: 2026-06-07T00:00:00Z
plan_path: ${PLAN}
target_size: M
auto_merge_approved: false
claude_transcript_id: tid-${SID}
---
# Target Session State
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${SID}"
target_claim_ttl: "2h"
MAN

# Stub `fno`: dispatch on the verb words. claim status returns the holder the
# precondition expects; agents spawn returns a JSON receipt; agents list reports
# the child live; everything else is a benign success.
cat > "${BIN_DIR}/fno" <<ABIEOF
#!/usr/bin/env bash
case "\$1 \$2" in
  "claim status")
    printf '{"holder":"target-session:${SID}","state":"live"}\n' ;;
  "claim acquire"|"claim release"|"claim refresh") exit 0 ;;
  "event emit") exit 0 ;;
  "agents spawn")
    printf '{"name":"tgt-child","short_id":"cx-abc123","provider":"claude","status":"live"}\n' ;;
  "agents list")
    printf '{"agents":[{"name":"tgt-${NODE_ID:3:8}-claude-g2","status":"live"}]}\n' ;;
  *) exit 0 ;;
esac
ABIEOF
chmod +x "${BIN_DIR}/fno"

# Stub `fno-agents`: record any `finalize` invocation (args verbatim).
FIN_MARKER="${TMP_DIR}/finalize_called"
cat > "${BIN_DIR}/fno-agents" <<AGEOF
#!/usr/bin/env bash
if [[ "\$1" == "finalize" ]]; then
  shift
  printf 'finalize %s\n' "\$*" >> "${FIN_MARKER}"
fi
exit 0
AGEOF
chmod +x "${BIN_DIR}/fno-agents"

# Run handoff at the blueprint->do boundary (skips the wave pressure probe).
OUT=""
RC=0
OUT=$(
  cd "$PROJ" || exit 99
  env HOME="$HOME_DIR" \
      PATH="${BIN_DIR}:${PATH}" \
      FNO_DIR="$FNO_DIR" \
      FNO_AGENTS_BIN="${BIN_DIR}/fno-agents" \
      CLAUDE_CODE_SESSION_ID="hsess-claude" \
      CODEX_THREAD_ID="" CODEX_SESSION_ID="" GEMINI_SESSION_ID="" \
      bash "$HANDOFF" --boundary blueprint-do 2>"${TMP_DIR}/handoff.stderr"
) || RC=$?

ARCHIVED="${PLAN}.artifacts/target-state-${SID}.md"

# ── assertions ───────────────────────────────────────────────────────────────
if [[ "$RC" -eq 0 ]] && printf '%s' "$OUT" | grep -q "^delegated ${NODE_ID}"; then
    pass "handoff committed the delegation (exit 0, delegated line)"
else
    fail "handoff did not delegate cleanly (rc=$RC, out='$OUT', stderr=$(tail -3 "${TMP_DIR}/handoff.stderr" 2>/dev/null))"
fi

if [[ -f "$FIN_MARKER" ]]; then
    pass "finalize was invoked by handoff at the commit point"
else
    fail "finalize was NOT invoked (no delegated ledger record)"
fi

if [[ -f "$FIN_MARKER" ]] && grep -q -- "--reason delegated" "$FIN_MARKER"; then
    pass "delegated ledger record uses --reason delegated (ledger-only, no stamp)"
else
    fail "finalize not called with --reason delegated: $(cat "$FIN_MARKER" 2>/dev/null)"
fi

if [[ -f "$FIN_MARKER" ]] && grep -qF -- "--state ${ARCHIVED}" "$FIN_MARKER"; then
    pass "finalize reads the ARCHIVED manifest (shim's finalize can't; this is why)"
else
    fail "finalize --state did not point at the archived manifest ($ARCHIVED): $(cat "$FIN_MARKER" 2>/dev/null)"
fi

# The original manifest must be gone (archived), proving the shim would skip.
if [[ ! -f "${FNO_DIR}/target-state.md" && -f "$ARCHIVED" ]]; then
    pass "manifest archived (shim hits the missing-manifest early-exit; no double-write)"
else
    fail "manifest archival invariant broken"
fi

printf '[handoff-ledger] Results: %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -gt 0 ]] && exit 1
exit 0

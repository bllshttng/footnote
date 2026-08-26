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
# Strategy: drive the real capability-escalation transaction end-to-end in a
# sandbox with stubbed `fno` (claim/spawn/truth/target/event) and a stub
# `fno-agents` that records its `finalize` invocation. Then assert handoff
# committed and finalized against the archived parent manifest.

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
CHILD_SID="00000000-0000-0000-0000-000000abc123"
CHILD_NAME="target-${NODE_ID}-work-g2"
CAP_NONCE="ledger-capability-nonce"
CAP_DIGEST="$(printf '%s\n%s\n%s' "$CAP_NONCE" "$PROJ" "$PROJ" | shasum -a 256 | awk '{print $1}')"

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

# Stub `fno`: the capability probe returns an exact nonce response. Raw target
# delivery installs the child claim and manifest, the commit proof handoff.sh
# requires before it emits delegated.
cat > "${BIN_DIR}/fno" <<ABIEOF
#!/usr/bin/env bash
if [[ "\${1:-} \${2:-}" == "agents claim" ]]; then
  shift
fi
case "\$1 \$2" in
  "claim status")
    if [[ -f "${TMP_DIR}/child-active" ]]; then
      printf '{"holder":"target-session:${CHILD_SID}","state":"live"}\n'
    else
      printf '{"holder":"target-session:${SID}","state":"live"}\n'
    fi ;;
  "claim acquire"|"claim release"|"claim refresh") exit 0 ;;
  "agents spawn")
    printf '{"name":"${CHILD_NAME}","short_id":"abc123","session_id":"${CHILD_SID}","harness":"claude","status":"live","bound":true,"readiness":"ready","model":"opus"}\n' ;;
  "agents truth")
    printf '{"state":"your-move","last_message":"FNO_CAPABILITY_READY:${CAP_DIGEST}","observed_model":{"kind":"observed","model":"opus","samples":1}}\n' ;;
  "agents mail")
    touch "${TMP_DIR}/child-active"
    cat > .fno/target-state.md <<'CHILD'
---
session_id: child-run
harness_session_id: ${CHILD_SID}
---
# Target Session State
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${CHILD_SID}"
CHILD
    printf 'delivered (hosted) to ${CHILD_SID}\n' ;;
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

# Run one explicit capability escalation.
OUT=""
RC=0
OUT=$(
  cd "$PROJ" || exit 99
  env HOME="$HOME_DIR" \
      PATH="${BIN_DIR}:${PATH}" \
      FNO_DIR="$FNO_DIR" \
      FNO_AGENTS_BIN="${BIN_DIR}/fno-agents" \
      HANDOFF_CAPABILITY_NONCE="$CAP_NONCE" \
      HANDOFF_CAPABILITY_EXPECTED_CWD="$PROJ" \
      HANDOFF_CAPABILITY_EXPECTED_ROOT="$PROJ" \
      CLAUDE_CODE_SESSION_ID="hsess-claude" \
      CODEX_THREAD_ID="" CODEX_SESSION_ID="" GEMINI_SESSION_ID="" \
      bash "$HANDOFF" --harness claude --model opus 2>"${TMP_DIR}/handoff.stderr"
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

# The parent manifest is archived while the current path belongs to the child.
if [[ -f "${FNO_DIR}/target-state.md" && -f "$ARCHIVED" ]] \
    && grep -q "harness_session_id: ${CHILD_SID}" "${FNO_DIR}/target-state.md"; then
    pass "parent manifest archived and current manifest belongs to child"
else
    fail "manifest archival invariant broken"
fi

printf '[handoff-ledger] Results: %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -gt 0 ]] && exit 1
exit 0

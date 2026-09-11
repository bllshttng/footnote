#!/usr/bin/env bash
# test_king_delegation_guard.sh
#
# Unit tests for hooks/king-delegation-guard.sh: a crowned court session is
# refused source authorship with a reason naming the delegation verbs (AC1);
# the unblock allowlist, the plans-directory carveout, and the off knob allow
# (AC2); a pass shape, an uncrowned row, and an unreadable registry all allow,
# the unreadable case with a line on stderr (AC3). The registry row, reign
# manifest, knob and plans dir are stubbed per case; no real fno state.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
KGD="$REPO_ROOT/hooks/king-delegation-guard.sh"

[[ -f "$KGD" ]] || { echo "FAIL: guard not found at $KGD" >&2; exit 1; }
export CLAUDE_PLUGIN_ROOT="$REPO_ROOT"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t king-delegation-guard-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Stub `fno`: registry-json from a fixture, manifest-path from a fixture file,
# the knob from a fixture, plan path from a fixture dir. Empty knob fixture
# resolves to the refuse default, exactly like an unset config key.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno" <<'STUB'
#!/usr/bin/env bash
if [ "$1" = "agents" ] && [ "$2" = "registry-json" ]; then
  cat "$KGD_REG_FIXTURE"
elif [ "$1" = "agents" ] && [ "$2" = "king" ] && [ "$3" = "manifest-path" ]; then
  printf '%s\n' "$*" >> "$KGD_MANIFEST_ARGS"
  if [ -n "$KGD_MANIFEST" ] && [ -f "$KGD_MANIFEST" ]; then
    echo "$KGD_MANIFEST"
  else
    exit 1
  fi
elif [ "$1" = "config" ] && [ "$2" = "get" ]; then
  cat "$KGD_KNOB" 2>/dev/null || true
elif [ "$1" = "config" ] && [ "$2" = "paths" ] && [ "$3" = "handoff" ]; then
  echo "$KGD_HANDOFF"
elif [ "$1" = "do" ] && [ "$2" = "plan" ] && [ "$3" = "path" ]; then
  echo "$KGD_PLANS/probe.md"
else
  exit 1
fi
STUB
chmod +x "$TMP/bin/fno"
export PATH="$TMP/bin:$PATH"
export KGD_REG_FIXTURE="$TMP/registry.json"
export KGD_KNOB="$TMP/knob.txt"
export KGD_PLANS="$TMP/plans"
export KGD_MANIFEST_ARGS="$TMP/manifest-args.log"
export KGD_HANDOFF="$TMP/handoffs/20260910-crown-fno.md"
mkdir -p "$KGD_PLANS"
: > "$KGD_KNOB"
: > "$KGD_MANIFEST_ARGS"

SID="sess-king"
SRC_FILE="$TMP/repo/src/main.py"

# registry_fixture <row-json>; manifest_fixture <shape> [sid]
registry_fixture() { printf '{"agents":[%s]}\n' "$1" > "$KGD_REG_FIXTURE"; }
manifest_fixture() {
  local shape="$1" mside="${2:-$SID}"
  export KGD_MANIFEST="$TMP/reign-manifest.md"
  printf 'scope: fno\nshape: %s\nharness_session_id: %s\n' "$shape" "$mside" > "$KGD_MANIFEST"
}
clear_manifest() { export KGD_MANIFEST=""; }

run_guard() { # $1 = payload JSON; stderr lands in $ERR via RUN_GUARD_ERR
  printf '%s' "$1" | bash "$KGD" 2>"$TMP/stderr.txt"
}

CROWNED='{"session_id":"'"$SID"'","harness_session_id":"full-'"$SID"'","crown_level":1,"crown_scope":"fno"}'
UNCROWNED='{"session_id":"'"$SID"'","harness_session_id":"full-'"$SID"'","crown_level":null,"crown_scope":null}'

edit_payload() { printf '{"tool_name":"Edit","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","old_string":"a","new_string":"b"}}' "$SID" "$TMP/repo" "$1"; }
bash_payload() { printf '{"tool_name":"Bash","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"command":"%s"}}' "$SID" "$TMP/repo" "$1"; }

# ── AC1-HP: crowned court + Edit on a source file -> deny naming both verbs ──
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
if [[ $RC -eq 0 ]] && echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
   && echo "$OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason' | grep -q "fno agents spawn '/fno:target <id>' --node <id> --substrate thread" \
   && echo "$OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason' | grep -q "fno backlog advance" \
   && echo "$OUT" | jq -e '.decision == "block"' >/dev/null 2>&1; then
  pass "AC1: crowned court Edit denied, reason names spawn + advance"
else
  fail "AC1: rc=$RC out=${OUT:0:300}"
fi

# ── AC2-EDGE: allowlist, plans carveout, knob off ─────────────────────────────
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(bash_payload "fno agents claim release node:x-1")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: claim release allowed" \
  || fail "AC2: claim release rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "fno backlog note x-1 evidence here")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: backlog lever allowed" \
  || fail "AC2: backlog lever rc=$RC out=$OUT"

OUT="$(run_guard "$(printf '{"tool_name":"Write","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","content":"plan"}}' "$SID" "$TMP/repo" "$KGD_PLANS/20260909-quick.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: plans-dir Write allowed" \
  || fail "AC2: plans-dir Write rc=$RC out=$OUT"

# NotebookEdit carries notebook_path, not file_path; the carveout must read it.
OUT="$(run_guard "$(printf '{"tool_name":"NotebookEdit","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"notebook_path":"%s","new_source":"x"}}' "$SID" "$TMP/repo" "$KGD_PLANS/book.ipynb")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: plans-dir NotebookEdit allowed via notebook_path" \
  || fail "AC2: plans-dir NotebookEdit rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "echo x > $KGD_PLANS/plan.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: plans-dir redirect allowed" \
  || fail "AC2: plans-dir redirect rc=$RC out=$OUT"

printf 'off\n' > "$KGD_KNOB"
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: knob off allows the AC1 Edit" \
  || fail "AC2: knob off rc=$RC out=$OUT"
: > "$KGD_KNOB"

printf 'warn\n' > "$KGD_KNOB"
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
ERR="$(cat "$TMP/stderr.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && "$ERR" == *"crowned court session does not implement"* ]] \
  && pass "AC2: knob warn emits refusal on stderr and allows" \
  || fail "AC2: knob warn rc=$RC out=$OUT err=$ERR"
: > "$KGD_KNOB"

# ── AC3-EDGE: pass shape, uncrowned row, no row, unreadable registry ─────────
registry_fixture "$CROWNED"
manifest_fixture pass
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC3: pass shape allows" \
  || fail "AC3: pass shape rc=$RC out=$OUT"

registry_fixture "$UNCROWNED"
clear_manifest
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC3: uncrowned row allows" \
  || fail "AC3: uncrowned row rc=$RC out=$OUT"

registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(printf '{"tool_name":"Edit","session_id":"sess-nobody","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s"}}' "$TMP/repo" "$SRC_FILE")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC3: no registry row allows" \
  || fail "AC3: no registry row rc=$RC out=$OUT"

registry_fixture ""
# Unreadable, not empty: an empty agents array is a valid registry (the silent
# no-row path above). A missing fixture file makes the stubbed registry-json
# itself fail, which is the unreadable path AC3 requires stderr on.
export KGD_REG_FIXTURE="$TMP/registry-missing.json"
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
ERR="$(cat "$TMP/stderr.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && -n "$ERR" ]] \
  && pass "AC3: unreadable registry allows with a stderr line" \
  || fail "AC3: unreadable registry rc=$RC out=$OUT err=$ERR"

# ── Guard's own Bash floor: source writes refused, verb calls allowed ────────
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(bash_payload "echo x > $TMP/repo/src/evil.py")")"; RC=$?
if [[ $RC -eq 0 ]] && echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
  pass "Bash floor: redirect into source denied"
else
  fail "Bash floor: redirect rc=$RC out=${OUT:0:300}"
fi

OUT="$(run_guard "$(bash_payload "cp a.py $TMP/repo/src/evil.py")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "Bash floor: cp into source denied" \
  || fail "Bash floor: cp rc=$RC out=${OUT:0:300}"

OUT="$(run_guard "$(bash_payload "fno agents mail send hi --to-self")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "Bash floor: mail verb allowed" \
  || fail "Bash floor: mail verb rc=$RC out=$OUT"

# tee writes EVERY FILE operand; a first-operand-only floor reads a plan path
# and approves while the second operand overwrites source.
OUT="$(run_guard "$(bash_payload "echo x | tee $KGD_PLANS/plan.md $TMP/repo/src/evil.py")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "Bash floor: tee with a second source target denied" \
  || fail "Bash floor: tee rc=$RC out=${OUT:0:300}"

# Quoting survives the floor: a legal plan write with quotes and spaces allows.
OUT="$(run_guard "$(bash_payload "cat > \"$KGD_PLANS/quoted plan.md\"")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "Bash floor: quoted plans-dir redirect allowed" \
  || fail "Bash floor: quoted redirect rc=$RC out=$OUT"

# Delegation with a thrown-away stderr is the guard's own remedy; /dev writes
# no source.
OUT="$(run_guard "$(bash_payload "fno agents spawn '/fno:target x-9' --node x-9 --substrate thread 2>/dev/null")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "Bash floor: spawn with 2>/dev/null allowed" \
  || fail "Bash floor: spawn devnull rc=$RC out=$OUT"

# ── Handoff exemption: the crown's own canon doc stays writable ──────────────
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(printf '{"tool_name":"Write","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","content":"gaps"}}' "$SID" "$TMP/repo" "$KGD_HANDOFF")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "handoff: Write to the crown's canon doc allowed" \
  || fail "handoff Write rc=$RC out=$OUT"

OUT="$(run_guard "$(edit_payload "$KGD_HANDOFF")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "handoff: Edit of the crown's canon doc allowed" \
  || fail "handoff Edit rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "cat >> $KGD_HANDOFF")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "handoff: cat redirect append allowed" \
  || fail "handoff cat rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "printf ruling | tee -a $KGD_HANDOFF")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "handoff: tee append allowed" \
  || fail "handoff tee rc=$RC out=$OUT"

# A sibling under the same directory is NOT the resolved doc: still denied.
OUT="$(run_guard "$(bash_payload "echo x > $TMP/handoffs/evil.md")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "handoff: sibling path still denied" \
  || fail "handoff sibling rc=$RC out=${OUT:0:300}"

OUT="$(run_guard "$(bash_payload "sed -i s/a/b/ $TMP/repo/src/x.py")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "Bash floor: in-place sed on source denied" \
  || fail "Bash floor: sed -i rc=$RC out=${OUT:0:300}"

# Positive control on the harness itself: the stub fno must be reachable and
# the crown read live, else every "allow" above is a silent stub failure.
command -v fno >/dev/null 2>&1 \
  && pass "positive control: stubbed fno on PATH" \
  || fail "positive control: stubbed fno missing"

# The manifest resolves through the CLI's canonical space root: the hook must
# NOT override --state-root with the checkout's .fno, which only a legacy
# layout has (on the documented default the override resolves nothing and the
# guard silently no-ops).
if grep -q -- "--state-root" "$KGD_MANIFEST_ARGS"; then
  fail "manifest-path called with a --state-root override: $(cat "$KGD_MANIFEST_ARGS")"
else
  pass "manifest-path resolves the CLI default (no --state-root override)"
fi

echo ""
echo "king-delegation-guard: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]

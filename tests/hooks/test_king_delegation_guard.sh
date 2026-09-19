#!/usr/bin/env bash
# Unit tests for hooks/king-delegation-guard.sh: a crowned court session is
# refused Edit/Write/NotebookEdit and shell writes to SOURCE. The predicate is
# inverted (2026-09-17 ruling): SOURCE is any path realpath-inside the repo
# root except the repo's .fno state tree; everything outside the repo - the
# vault wherever it lives, the crown handoff doc, escalations notes,
# auto-memory - allows, and there is no enumeration of exempt trees anymore.
#
# The policy moved into crates/fno-agents/src/hook/king_guard.rs and
# the script became a probe-and-relay wrapper (never exec: a candidate that
# lacks the hook verb falls through instead of refusing every tool), so the
# fixtures are real files the native guard reads (registry.json under
# FNO_AGENTS_HOME, the court manifest under the space's kings/, config.toml at
# the payload cwd) instead of stubbed verb outputs. Every pre-port case keeps
# its semantics; the stub positive control became a no-subprocess canary (the
# native guard must spawn no `fno`).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
KGD="$REPO_ROOT/hooks/king-delegation-guard.sh"
[[ -f "$KGD" ]] || { echo "FAIL: guard not found at $KGD" >&2; exit 1; }
# Same resolution order as the wrapper: PATH, env, release, debug. A sibling leg
# of the packet (preflight, the cargo-isolation tests) may have cleaned the
# target dir between provisioning and this run: rebuild the debug binary
# quietly rather than fail on an artifact the environment is documented to
# provide.
BIN="${FNO_AGENTS_BIN:-}"
if [[ -z "$BIN" ]]; then
    for candidate in "$REPO_ROOT/crates/fno-agents/target/release/fno-agents" \
        "$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"; do
        [[ -x "$candidate" ]] && BIN="$candidate" && break
    done
fi
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
    (cd "$REPO_ROOT/crates/fno-agents" && cargo build --bin fno-agents >/dev/null 2>&1)
    BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"
fi
[[ -x "$BIN" ]] || { echo "FAIL: fno-agents binary not executable at $BIN" >&2; exit 1; }

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  PASS: $*"; }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t king-delegation-guard-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Real fixture files under the pinned roots the native guard reads.
export FNO_AGENTS_HOME="$TMP/home/agents"
export FNO_EVENTS_PATH="$TMP/space/events.jsonl"
export FNO_SPACES_DIR="$TMP/spaces"
mkdir -p "$FNO_AGENTS_HOME" "$TMP/space/kings" "$TMP/repo/.fno"

# A `fno` canary: the native guard never shells out, so this must never run.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$KGD_FNO_CALLS"
exit 1
STUB
chmod +x "$TMP/bin/fno"
# The wrapper tries the deployed PATH binary first; pin that slot to the same
# BIN verified below so the suite tests policy, not the operator's installed
# version.
mkdir -p "$TMP/realbin"
ln -s "$BIN" "$TMP/realbin/fno-agents"
export PATH="$TMP/realbin:$PATH"
export KGD_FNO_CALLS="$TMP/fno-calls.log"
: > "$KGD_FNO_CALLS"

# The repo config: knob, plans dir, handoffs dir, and the vault pin that
# resolves the escalations dir ($HOME/fixture-vault/internal/repo/escalations).
mkdir -p "$TMP/home/fixture-vault"
cat > "$TMP/repo/.fno/config.toml" <<EOF
plans_dir = "$TMP/plans"
[paths]
handoffs_dir = "$TMP/handoffs"
[obsidian]
enabled = true
vault = "fixture-vault"
EOF
mkdir -p "$TMP/plans" "$TMP/handoffs"

SID="sess-king"
SRC_FILE="$TMP/repo/src/main.py"
mkdir -p "$TMP/repo/src"

registry_fixture() { printf '{"schema_version":26,"agents":[%s]}\n' "$1" > "$FNO_AGENTS_HOME/registry.json"; }
manifest_fixture() {
  local shape="$1" mside="${2:-$SID}"
  printf -- '---\nfno_id: 20260915T190000Z-kg1-abcdef\nscope: fno\nshape: %s\nharness_session_id: %s\n---\n' "$shape" "$mside" \
    > "$TMP/space/kings/fno.md"
}
clear_manifest() { rm -f "$TMP/space/kings/fno.md"; }
set_knob() { # empty -> unset (refuse default); else refuse|warn|off
  if [[ -z "$1" ]]; then
    sed -i '' '/implementation_guard/d' "$TMP/repo/.fno/config.toml" 2>/dev/null \
      || sed -i '/implementation_guard/d' "$TMP/repo/.fno/config.toml"
  else
    printf '[king]\nimplementation_guard = "%s"\n' "$1" >> "$TMP/repo/.fno/config.toml"
  fi
}
clear_knob() {
  sed -i '' '/implementation_guard/d; /^\[king\]$/d' "$TMP/repo/.fno/config.toml" 2>/dev/null \
    || sed -i '/implementation_guard/d; /^\[king\]$/d' "$TMP/repo/.fno/config.toml"
}

run_guard() { # $1 = payload JSON; stderr lands in $ERR via RUN_GUARD_ERR
  printf '%s' "$1" | bash "$KGD" 2>"$TMP/stderr.txt"
}

CROWNED='{"name":"fixture-king","status":"live","cwd":"'"$TMP/repo"'","created_at":"2026-09-15T19:00:00Z","session_id":"'"$SID"'","harness":"claude","harness_session_id":"full-'"$SID"'","crown_level":1,"crown_scope":"fno"}'
UNCROWNED='{"name":"fixture-king","status":"live","cwd":"'"$TMP/repo"'","created_at":"2026-09-15T19:00:00Z","session_id":"'"$SID"'","harness":"claude","harness_session_id":"full-'"$SID"'"}'

edit_payload() { printf '{"tool_name":"Edit","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","old_string":"a","new_string":"b"}}' "$SID" "$TMP/repo" "$1"; }
bash_payload() { printf '{"tool_name":"Bash","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"command":"%s"}}' "$SID" "$TMP/repo" "$1"; }
edit_payload_t() { printf '{"tool_name":"Edit","session_id":"%s","transcript_path":"%s","cwd":"%s","tool_input":{"file_path":"%s","old_string":"a","new_string":"b"}}' "$SID" "$2" "$TMP/repo" "$1"; }
# $1 file, $2 transcript_path, $3 agent_id - the subagent-borne shape carries
# the parent session id plus the harness's per-call subagent marker.
edit_payload_ag() { printf '{"tool_name":"Edit","session_id":"%s","transcript_path":"%s","agent_id":"%s","cwd":"%s","tool_input":{"file_path":"%s","old_string":"a","new_string":"b"}}' "$SID" "$2" "$3" "$TMP/repo" "$1"; }

# Escalations resolve through the vault pin (project name = the git repo's
# basename). Seed the repo as git so resolve_project_name answers "repo".
/usr/bin/git init -q "$TMP/repo"
ESCALATIONS="$TMP/home/fixture-vault/internal/repo/escalations"
mkdir -p "$ESCALATIONS"
# Pre-existing crown handoff doc: the resolver takes the newest *-crown-fno.md.
KGD_HANDOFF="$TMP/handoffs/20260910-crown-fno.md"
printf 'gaps:\n' > "$KGD_HANDOFF"

# ── AC1-HP: crowned court + Edit on a source file -> deny names path + roots ─
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
REASON="$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty')"
if [[ $RC -eq 0 ]] && echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
   && printf '%s' "$REASON" | grep -qF "$SRC_FILE" \
   && printf '%s' "$REASON" | grep -qF "does not write SOURCE" \
   && printf '%s' "$REASON" | grep -qF "inside the repo" \
   && ! printf '%s' "$REASON" | grep -qiE "spawn|advance" \
   && echo "$OUT" | jq -e '.decision == "block"' >/dev/null 2>&1; then
  pass "AC1: crowned court Edit denied, reason names the path + the source rule, no delegation verbs"
else
  fail "AC1: rc=$RC out=${OUT:0:300} err=$(cat "$TMP/stderr.txt")"
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

OUT="$(run_guard "$(printf '{"tool_name":"Write","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","content":"plan"}}' "$SID" "$TMP/repo" "$TMP/plans/20260909-quick.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: plans-dir Write allowed" \
  || fail "AC2: plans-dir Write rc=$RC out=$OUT"

# NotebookEdit carries notebook_path, not file_path; the carveout must read it.
OUT="$(run_guard "$(printf '{"tool_name":"NotebookEdit","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"notebook_path":"%s","new_source":"x"}}' "$SID" "$TMP/repo" "$TMP/plans/book.ipynb")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: plans-dir NotebookEdit allowed via notebook_path" \
  || fail "AC2: plans-dir NotebookEdit rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "echo x > $TMP/plans/plan.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: plans-dir redirect allowed" \
  || fail "AC2: plans-dir redirect rc=$RC out=$OUT"

set_knob off
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "AC2: knob off allows the AC1 Edit" \
  || fail "AC2: knob off rc=$RC out=$OUT"
clear_knob

set_knob warn
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
ERR="$(cat "$TMP/stderr.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && "$ERR" == *"$SRC_FILE"* && "$ERR" == *"does not write SOURCE"* ]] \
  && pass "AC2: knob warn names the path on stderr and allows" \
  || fail "AC2: knob warn rc=$RC out=$OUT err=$ERR"
clear_knob

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

# Unreadable, not empty: an empty agents array is a valid registry (the silent
# no-row path above). A registry that fails to parse must allow with a stderr
# line - never a silent no-owner.
printf 'not json at all' > "$FNO_AGENTS_HOME/registry.json"
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
OUT="$(run_guard "$(bash_payload "echo x | tee $TMP/plans/plan.md $TMP/repo/src/evil.py")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "Bash floor: tee with a second source target denied" \
  || fail "Bash floor: tee rc=$RC out=${OUT:0:300}"

# Quoting survives the floor: a legal plan write with quotes and spaces allows.
OUT="$(run_guard "$(bash_payload "cat > \"$TMP/plans/quoted plan.md\"")")"; RC=$?
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

# A sibling under the same directory is not the resolved doc; under the
# inverted predicate the boundary is the repo, so an out-of-repo sibling
# allows while its in-repo twin stays source.
OUT="$(run_guard "$(bash_payload "echo x > $TMP/handoffs/evil.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "handoff: out-of-repo sibling of the canon doc allows" \
  || fail "handoff sibling rc=$RC out=${OUT:0:300}"

OUT="$(run_guard "$(bash_payload "echo x > $TMP/repo/handoffs/evil.md")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "handoff: in-repo sibling of the doc name stays denied" \
  || fail "handoff in-repo sibling rc=$RC out=${OUT:0:300}"

OUT="$(run_guard "$(bash_payload "sed -i s/a/b/ $TMP/repo/src/x.py")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "Bash floor: in-place sed on source denied" \
  || fail "Bash floor: sed -i rc=$RC out=${OUT:0:300}"

# ── Glued boundary: shlex glues `;` onto a redirect token when unspaced ──────
# `2>&1;` must still classify as the fd dup, not a write to a file named `&1;`.
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(bash_payload "echo probe-d 2>&1; echo probe-e")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "glue: chained 2>&1; allows" \
  || fail "glue chained rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "echo probe-i 2>&1 ; echo probe-j")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "glue: spaced control allows" \
  || fail "glue spaced rc=$RC out=$OUT"

# A real target outside plans still denies, semicolon glued or not.
OUT="$(run_guard "$(bash_payload "echo x > $TMP/repo/src/evil.py; echo done")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "glue: real source target glued to ; denied" \
  || fail "glue real target rc=$RC out=${OUT:0:300}"

# A quoted semicolon inside the filename is data: plans-dir write still allows.
OUT="$(run_guard "$(bash_payload "cat > \"$TMP/plans/plan;.md\"")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "glue: quoted plan;.md in plans allows" \
  || fail "glue quoted rc=$RC out=$OUT"

# The node's live specimen shape: a read verb with a thrown-away dup chain.
OUT="$(run_guard "$(bash_payload "fno backlog session close x-1 --launch '/fno:target x-1' 2>&1; echo done")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "glue: session close with 2>&1; chain allows" \
  || fail "glue session close rc=$RC out=$OUT"

# ── Node specimens: reads pass untouched; a refusal names the path, never ────
# delegation. These are the verify shapes the node names.
registry_fixture "$CROWNED"
manifest_fixture court
OUT="$(run_guard "$(bash_payload "git log --oneline -5")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "reads: git log allows" \
  || fail "reads git log rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "ls -la $TMP/repo/src")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "reads: ls allows" \
  || fail "reads ls rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "cp $TMP/brief.md $KGD_HANDOFF")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "handoff: cp into the canon doc allowed" \
  || fail "handoff cp rc=$RC out=$OUT"

mkdir -p "$TMP/vaultdir/briefs"
OUT="$(run_guard "$(bash_payload "cp $TMP/brief.md $TMP/vaultdir/briefs/b.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "vault: cp to a tree outside the repo allows" \
  || fail "vault cp rc=$RC out=${OUT:0:300}"

# The cp-shaped deny survives for an in-repo destination: source, named.
OUT="$(run_guard "$(bash_payload "cp $TMP/brief.md $TMP/repo/src/evil.py")")"; RC=$?
REASON="$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty')"
if echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
   && printf '%s' "$REASON" | grep -qF "repo/src/evil.py" \
   && ! printf '%s' "$REASON" | grep -qiE "spawn|advance"; then
  pass "vault: cp into the repo denied, reason names the path, no delegation verbs"
else
  fail "vault cp in-repo rc=$RC out=${OUT:0:300}"
fi

# ── Limb carve-out: a Task subagent of this very court is a limb, not the king.
# Its payload carries the parent's session_id, so sections 2-4 see the crown,
# plus a non-empty agent_id, the per-call subagent marker; the transcript path
# names the parent transcript for king and limb alike, so the on-disk
# subagents/ layout is only the second signature.
SUBTRANS="$TMP/transcripts/$SID/subagents/agent-x.jsonl"
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$SUBTRANS")")"; RC=$?
ERR="$(cat "$TMP/stderr.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && "$ERR" == *"limb of crowned session $SID"* ]] \
  && pass "limb: subagent transcript Write allowed, stderr names the limb" \
  || fail "limb allow rc=$RC out=$OUT err=$ERR"

# The live incident shape: the payload's transcript_path names the PARENT main
# transcript (measured: it is never the limb's subagents file), and only the
# agent_id marks the call as subagent-borne. This blocked a real limb on
# 2026-09-13; it must allow.
MAINTRANS="$TMP/transcripts/$SID/main.jsonl"
OUT="$(run_guard "$(edit_payload_ag "$SRC_FILE" "$MAINTRANS" "agent-a4f5701e9783b4bfe")")"; RC=$?
ERR="$(cat "$TMP/stderr.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && "$ERR" == *"limb (agent_id agent-a4f5701e9783b4bfe) of crowned session $SID"* ]] \
  && pass "limb: agent_id allows with a parent main transcript, stderr names the agent" \
  || fail "limb agent_id rc=$RC out=$OUT err=$ERR"

# A bg job limb may carry an empty transcript_path entirely; agent_id still
# decides. Named and unnamed limbs both carry it.
OUT="$(run_guard "$(edit_payload_ag "$SRC_FILE" "" "agent-hotfix-restart-json")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "limb: agent_id allows with an empty transcript (bg job shape)" \
  || fail "limb agent_id empty-transcript rc=$RC out=$OUT"

# A foreign session's subagents dir is not this court's limb: fail closed.
FOREIGNTRANS="$TMP/transcripts/sess-other/subagents/agent-y.jsonl"
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$FOREIGNTRANS")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "limb: foreign session's subagents transcript denied" \
  || fail "limb foreign rc=$RC out=${OUT:0:300}"

# A main-thread transcript (parent dir is not subagents/) and no agent_id
# keeps court treatment.
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$MAINTRANS")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "limb: main-thread transcript denied" \
  || fail "limb main-thread rc=$RC out=${OUT:0:300}"

# Empty transcript (non-claude harness) keeps court treatment: AC1 already pins
# it, re-asserted here next to the carve-out cases.
OUT="$(run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "limb: empty transcript denied" \
  || fail "limb empty rc=$RC out=${OUT:0:300}"

# ── Memory carve-out: auto-memory is not implementation. The live specimen:
# a crowned court refused a memory Write the operator asked for (measured
# 2026-09-14). HOME is redirected so the check runs against a fixture
# projects root.
export HOME="$TMP/home"
MEMPROJ="$HOME/.claude/projects/-Users-bb16-code-footnote-footnote"
MEMDIR="$MEMPROJ/memory"
mkdir -p "$MEMDIR"
OUT="$(run_guard "$(printf '{"tool_name":"Write","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","content":"ruling"}}' "$SID" "$TMP/repo" "$MEMDIR/feedback-codex-spawns-luna-not-astra.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "memory: court Write of a memory note allowed" \
  || fail "memory Write rc=$RC out=$OUT"

OUT="$(run_guard "$(edit_payload "$MEMDIR/MEMORY.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "memory: court Edit of MEMORY.md allowed" \
  || fail "memory Edit rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "echo ruling >> $MEMDIR/MEMORY.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "memory: Bash append into MEMORY.md allowed" \
  || fail "memory append rc=$RC out=$OUT"

# Under the inverted predicate the memory tree allows because it sits outside
# the repo - not because it is enumerated. The boundary lives inside the repo:
# a memory-shaped tree UNDER the repo root is still source.
OUT="$(run_guard "$(edit_payload "$MEMPROJ/notes.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "memory: project-dir sibling outside the repo allows" \
  || fail "memory sibling rc=$RC out=${OUT:0:300}"

OUT="$(run_guard "$(edit_payload "$TMP/repo/.claude/projects/stray.md")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "memory: repo-resident memory-lookalike stays denied" \
  || fail "memory stray rc=$RC out=${OUT:0:300}"

# ── Escalations: the superuser-tier lane a king files. Under the inverted
# predicate its notes allow because the vault sits outside the repo.
OUT="$(run_guard "$(printf '{"tool_name":"Write","session_id":"%s","transcript_path":"","cwd":"%s","tool_input":{"file_path":"%s","content":"note"}}' "$SID" "$TMP/repo" "$ESCALATIONS/20260915-0900-token.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "escalations: court Write of an escalation note allowed" \
  || fail "escalations Write rc=$RC out=$OUT"

OUT="$(run_guard "$(bash_payload "echo question >> $ESCALATIONS/20260915-0900-token.md")")"; RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] \
  && pass "escalations: Bash append into an escalation note allowed" \
  || fail "escalations append rc=$RC out=$OUT"

# A lookalike internal/ tree is denied the moment it lives INSIDE the repo:
# the guard matches the resolved path against the repo root, never a prefix
# of the string.
OUT="$(run_guard "$(edit_payload "$TMP/repo/internal/fno/decisions/x.md")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "escalations: in-repo lookalike tree denied" \
  || fail "escalations outside rc=$RC out=${OUT:0:300}"

OUT="$(run_guard "$(edit_payload "$TMP/repo/internal/fno/escalations/escape.md")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "escalations: in-repo lookalike escalations tree denied" \
  || fail "escalations lookalike rc=$RC out=${OUT:0:300}"

# ── Third limb signature: the live claude payload carries no agent_id and its
# transcript_path names the parent MAIN transcript (the refusal of 2026-09-14).
# The world marker is an open Task/Agent tool_use in that transcript.
PARENT_TRANS="$TMP/transcripts/$SID/main.jsonl"
mkdir -p "$(dirname "$PARENT_TRANS")"
printf '{"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_open01","name":"Agent","input":{"description":"Update codex model memory","subagent_type":"general-purpose"}}]}}\n' > "$PARENT_TRANS"
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$PARENT_TRANS")")"; RC=$?
ERR="$(cat "$TMP/stderr.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && "$ERR" == *"limb of crowned session $SID"* ]] \
  && pass "limb: open Agent tool_use in the parent transcript allows (live incident shape)" \
  || fail "limb open-agent rc=$RC out=$OUT err=$ERR"

# After the limb's tool_result lands, the same write is the king's again.
printf '{"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_open01","content":"done"}]}}\n' >> "$PARENT_TRANS"
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$PARENT_TRANS")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "limb: resolved Agent tool_use denies again" \
  || fail "limb resolved rc=$RC out=${OUT:0:300}"

# A transcript with no Agent/Task tool_use at all keeps court treatment.
printf '{"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_r1","name":"Read","input":{"file_path":"/tmp/x"}}]}}\n' > "$PARENT_TRANS"
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$PARENT_TRANS")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "limb: transcript with no spawn tool_use denied" \
  || fail "limb no-spawn rc=$RC out=${OUT:0:300}"

# An aborted spawn the king worked past has a LATER tool_use after it: it no
# longer holds the allowance, so the write is the king's own again (denied).
printf '{"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_open01","name":"Agent","input":{"description":"aborted"}}]}}\n{"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_r2","name":"Read","input":{"file_path":"/tmp/x"}}]}}\n' > "$PARENT_TRANS"
OUT="$(run_guard "$(edit_payload_t "$SRC_FILE" "$PARENT_TRANS")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "limb: orphaned open spawn with later tool_use denied" \
  || fail "limb orphan rc=$RC out=${OUT:0:300}"

# ── Stale-binary fallthrough: a binary without the hook verb must never ──────
# wedge the session. Live outage 2026-09-17: 19 worktree builds
# predated the verb; the old exec answered "unknown verb" and refused every
# tool in those sessions.
mkdir -p "$TMP/stalebin"
cat > "$TMP/stalebin/fno-agents" <<'STALE'
#!/usr/bin/env bash
printf '%s\n' "fno-agents: unknown verb: hook (expected --emit-schema|...)"
exit 2
STALE
chmod +x "$TMP/stalebin/fno-agents"

registry_fixture "$CROWNED"
manifest_fixture court

# A stale PATH binary falls through to the env override: the court Edit is
# still denied by a real policy decision, and the hook still exits 0.
export FNO_AGENTS_BIN="$BIN"
OUT="$(PATH="$TMP/stalebin:$PATH" run_guard "$(edit_payload "$SRC_FILE")")"; RC=$?
echo "$OUT" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 \
  && pass "stale: unknown-verb PATH binary falls through, court Edit still denied" \
  || fail "stale fallthrough rc=$RC out=${OUT:0:300}"
unset FNO_AGENTS_BIN

# A stale binary with no good binary behind it fail-opens: the session keeps
# its tools. This is the exact outage shape, which used to exit 2 on every
# call; run from a cwd with no in-tree build so nothing else can answer.
mkdir -p "$TMP/bare"
( cd "$TMP/bare" && printf '%s' "$(edit_payload "$SRC_FILE")" \
    | FNO_AGENTS_BIN= PATH="$TMP/stalebin:/usr/bin:/bin" bash "$KGD" ) \
    > "$TMP/stale-out.txt" 2>"$TMP/stale-err.txt"; RC=$?
OUT="$(cat "$TMP/stale-out.txt")"
ERR="$(cat "$TMP/stale-err.txt")"
[[ $RC -eq 0 && "$OUT" == "{}" && "$ERR" == *"allowing"* ]] \
  && pass "stale: no good binary fail-opens, session keeps its tools" \
  || fail "stale fail-open rc=$RC out=$OUT err=$ERR"

# The deployed PATH binary outranks an in-tree build even when the in-tree
# build is healthy: policy comes from the installed release, not the branch.
mkdir -p "$TMP/pathbin" "$TMP/repo/crates/fno-agents/target/debug"
cat > "$TMP/pathbin/fno-agents" <<'PATHSTUB'
#!/usr/bin/env bash
printf '%s\n' '{"stub":"path"}'
PATHSTUB
cat > "$TMP/repo/crates/fno-agents/target/debug/fno-agents" <<'DEBUGSTUB'
#!/usr/bin/env bash
printf '%s\n' '{"stub":"debug"}'
DEBUGSTUB
chmod +x "$TMP/pathbin/fno-agents" "$TMP/repo/crates/fno-agents/target/debug/fno-agents"
OUT="$( cd "$TMP/repo" && printf '%s' '{"tool_name":"Edit"}' \
    | PATH="$TMP/pathbin:$PATH" bash "$KGD" 2>/dev/null )"
[[ "$OUT" == '{"stub":"path"}' ]] \
  && pass "order: deployed PATH binary outranks a healthy in-tree build" \
  || fail "order rc out=$OUT"

# Positive control 1: the binary runs at all, so every PASS above is real.
"$BIN" version >/dev/null 2>&1 \
  && pass "positive control: fno-agents binary executes" \
  || fail "positive control: fno-agents binary failed to run"

# Positive control 2: the native guard spawns no `fno` process (the canary
# stays empty across every decision above). This is what replaces the stub
# positive control: the guard's five old CLI round trips are GONE.
[[ ! -s "$KGD_FNO_CALLS" ]] \
  && pass "positive control: no fno subprocess across all decisions" \
  || fail "positive control: guard shelled fno: $(cat "$KGD_FNO_CALLS")"

echo ""
echo "king-delegation-guard: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]

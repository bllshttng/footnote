#!/usr/bin/env bash
# Contract tests for hooks/inject-mail-notify.sh (native mail-notify-self path).
#
# Two layers:
#   stub cases - the shell gate logic: identity + bus + binary gates, the
#                byte-for-byte fd-3 relay, the budget-bounded hang, and miss
#                rows. A stub `fno-agents` answers; no build needed.
#   journeys   - the real binary end to end: envelope shape, defang, cursor
#                acknowledgement, second-boundary silence, the busy hold
#                short-circuit, and drained receipts. Skipped (77) when no
#                binary exists; the runner that greps this file for
#                target/debug/fno-agents owes the build step and re-runs it.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/inject-mail-notify.sh"
[[ -f "$HOOK" ]] || { echo "FAIL: hook missing at $HOOK"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

SID="ses_0123456789abcdef0123456789abcdef"   # full harness session id
HANDLE="ses_0123"                              # canonical first-eight handle
PASS=0
FAIL=0

ok()  { PASS=$((PASS + 1)); echo "ok: $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }

STATE="$TMP/state"          # FNO_STATE_DIR (hook bus) and FNO_HOME (hold root)
EVENTS="$TMP/events.jsonl"  # events.sh journal for miss rows
ARGS_LOG="$TMP/stub-args.log"

# Real fno, for reading recorded miss rows back via doctor event rows.
REAL_FNO=""
for c in "$REPO_ROOT/crates/fno/target/debug/fno" "$REPO_ROOT/crates/fno/target/release/fno"; do
    [[ -x "$c" ]] && { REAL_FNO="$c"; break; }
done
[[ -n "$REAL_FNO" ]] || REAL_FNO="$(command -v fno 2>/dev/null || true)"
[[ -n "$REAL_FNO" ]] || { echo "FAIL: no fno binary (needed to read miss rows)"; exit 1; }

# Stub fno-agents: logs argv, optional sleep/output/failure knobs.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno-agents" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FNO_STUB_ARGS_LOG"
if [[ "${FNO_STUB_SLEEP:-0}" != "0" ]]; then sleep "$FNO_STUB_SLEEP"; fi
if [[ "${FNO_STUB_FAIL:-0}" != "0" ]]; then
    [[ -n "${FNO_STUB_ERR:-}" ]] && printf '%s\n' "$FNO_STUB_ERR" >&2
    exit "${FNO_STUB_RC:-1}"
fi
if [[ -n "${FNO_STUB_OUT:-}" ]]; then printf '%s' "$FNO_STUB_OUT"; fi
exit "${FNO_STUB_RC:-0}"
STUB
chmod +x "$TMP/bin/fno-agents"

fresh_env() {
    rm -rf "$STATE"
    mkdir -p "$STATE/bus"
    : > "$EVENTS"
    : > "$ARGS_LOG"
}

seed_bus() {  # $@: raw JSONL message lines
    printf '%s\n' "$@" > "$STATE/bus/messages.jsonl"
}

STDIN_OK="$(printf '{"session_id":"%s","prompt":"go"}' "$SID")"

run_hook() {  # stdin JSON on $1; knobs come from the exported FNO_STUB_* set
    PATH="$TMP/bin:$PATH" \
    FNO_STATE_DIR="$STATE" FNO_HOME="$STATE" \
    EVENTS_FILE="$EVENTS" FNO_BIN="$REAL_FNO" \
    FNO_STUB_ARGS_LOG="$ARGS_LOG" \
    FNO_HOOK_BUDGET_SKIP_PER_CORE=1000000 \
    bash "$HOOK" <<<"$1"
}

read_miss_events() {
    "$REAL_FNO" doctor event rows --events "$EVENTS" --type mail_notify_self_missed
}

miss_row_holding() {
    read_miss_events | jq -e "any(.[]; (fromjson | .data | $1))"
}

ENVELOPE='{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"<system-reminder>\nhello\n</system-reminder>"}}'

# --- gate cases: silent exits leave the stub untouched ---------------------

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
out="$(run_hook '{"prompt":"no session id"}')" && gate_rc=0 || gate_rc=$?
if [[ "$gate_rc" -eq 0 && -z "$out" && ! -s "$ARGS_LOG" ]]; then
    ok "no session_id: silent exit, stub not called"
else
    bad "no session_id: rc=$gate_rc out=${#out} args=$(cat "$ARGS_LOG" 2>/dev/null)"
fi

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"shortid1234567","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
out="$(run_hook '{"session_id":"shortid1234567","prompt":"go"}')" && gate_rc=0 || gate_rc=$?
if [[ "$gate_rc" -eq 0 && -z "$out" && ! -s "$ARGS_LOG" ]]; then
    ok "short session_id: silent exit, stub not called"
else
    bad "short session_id: rc=$gate_rc out=${#out} args=$(cat "$ARGS_LOG" 2>/dev/null)"
fi

fresh_env
out="$(run_hook "$STDIN_OK")" && gate_rc=0 || gate_rc=$?
if [[ "$gate_rc" -eq 0 && -z "$out" && ! -s "$ARGS_LOG" ]]; then
    ok "no bus log: silent exit, stub not called"
else
    bad "no bus log: rc=$gate_rc out=${#out} args=$(cat "$ARGS_LOG" 2>/dev/null)"
fi

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
out="$(PATH="/usr/bin:/bin" FNO_STATE_DIR="$STATE" FNO_HOME="$STATE" \
    FNO_HOOK_BUDGET_SKIP_PER_CORE=1000000 bash "$HOOK" <<<"$STDIN_OK" 2>/dev/null)" && gate_rc=0 || gate_rc=$?
if [[ "$gate_rc" -eq 0 && -z "$out" && ! -s "$ARGS_LOG" ]] && [[ "$(read_miss_events)" == "[]" ]]; then
    ok "missing fno-agents on PATH: silent exit, no miss row (a gate skip is not a miss)"
else
    bad "missing fno-agents: rc=$gate_rc out=${#out} rows=$(read_miss_events)"
fi

# --- relay case: the verb's stdout IS the payload, byte-for-byte -----------

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
export FNO_STUB_OUT="$ENVELOPE"
out="$(run_hook "$STDIN_OK")" && relay_rc=0 || relay_rc=$?
unset FNO_STUB_OUT
expected_args="mail-notify-self --bus-dir $STATE/bus --session $SID"
if [[ "$relay_rc" -eq 0 && "$out" == "$ENVELOPE" && "$(cat "$ARGS_LOG")" == "$expected_args" ]]; then
    ok "relay: envelope byte-for-byte through fd 3, argv names the native verb"
else
    bad "relay: rc=$relay_rc args=$(cat "$ARGS_LOG" 2>/dev/null)"
fi

# --- failure then retry: a miss is recorded, the next boundary delivers ----

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
export FNO_STUB_FAIL=1 FNO_STUB_ERR="identity refused" FNO_STUB_RC=1
out="$(run_hook "$STDIN_OK")" && fail_rc=0 || fail_rc=$?
unset FNO_STUB_FAIL FNO_STUB_ERR FNO_STUB_RC
if [[ "$fail_rc" -eq 0 && -z "$out" ]] \
    && miss_row_holding '.rc == 1 and .stderr_tail == "identity refused"' >/dev/null; then
    ok "verb failure: turn proceeds, miss row names rc and stderr"
else
    bad "verb failure: rc=$fail_rc out=${#out} rows=$(read_miss_events)"
fi
export FNO_STUB_OUT="$ENVELOPE"
out="$(run_hook "$STDIN_OK")" && retry_rc=0 || retry_rc=$?
unset FNO_STUB_OUT
if [[ "$retry_rc" -eq 0 && "$out" == "$ENVELOPE" ]]; then
    ok "retry at the next boundary delivers"
else
    bad "retry: rc=$retry_rc out=${#out}"
fi

# --- hung binary: the hook budget bounds it, the miss row records 124 ------

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
NOCU_BIN="$TMP/nocu-bin"
mkdir -p "$NOCU_BIN"
for tool in bash sleep jq dirname; do
    ln -sf "$(command -v "$tool")" "$NOCU_BIN/$tool"
done
ln -sf "$TMP/bin/fno-agents" "$NOCU_BIN/fno-agents"
t0=$(date +%s)
out="$(PATH="$NOCU_BIN" FNO_STATE_DIR="$STATE" FNO_HOME="$STATE" \
    EVENTS_FILE="$EVENTS" FNO_BIN="$REAL_FNO" FNO_STUB_ARGS_LOG="$ARGS_LOG" \
    FNO_STUB_SLEEP=9 FNO_HOOK_BUDGET_SECS=2 FNO_HOOK_BUDGET_SKIP_PER_CORE=1000000 \
    bash "$HOOK" <<<"$STDIN_OK" 2>/dev/null)" && hang_rc=0 || hang_rc=$?
t1=$(date +%s)
elapsed=$((t1 - t0))
if [[ "$hang_rc" -eq 0 && -z "$out" && "$elapsed" -ge 1 && "$elapsed" -lt 6 ]] \
    && miss_row_holding '.rc == 124' >/dev/null; then
    ok "hung binary: budget kills at ${elapsed}s, turn proceeds, miss row rc 124"
else
    bad "hung binary: rc=$hang_rc elapsed=${elapsed}s rows=$(read_miss_events)"
fi

# --- broken jq: identity is unreadable, a deterministic miss row records it -

fresh_env
seed_bus '{"id":"m1","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"hi","ts":"2026-10-09T00:00:00Z"}'
NOJQ_BIN="$TMP/nojq-bin"
mkdir -p "$NOJQ_BIN"
printf '#!/bin/sh\nexit 127\n' > "$NOJQ_BIN/jq"
chmod +x "$NOJQ_BIN/jq"
out="$(PATH="$NOJQ_BIN:$TMP/bin:$PATH" FNO_STATE_DIR="$STATE" FNO_HOME="$STATE" \
    EVENTS_FILE="$EVENTS" FNO_BIN="$REAL_FNO" FNO_STUB_ARGS_LOG="$ARGS_LOG" \
    FNO_HOOK_BUDGET_SKIP_PER_CORE=1000000 \
    bash "$HOOK" <<<"$STDIN_OK" 2>/dev/null)" && nojq_rc=0 || nojq_rc=$?
if [[ "$nojq_rc" -eq 0 && -z "$out" && ! -s "$ARGS_LOG" ]] \
    && miss_row_holding '.rc == 127 and .stderr_tail == "jq not found; session id unreadable"' >/dev/null; then
    ok "broken jq: turn proceeds, stub not called, deterministic miss row rc 127"
else
    bad "broken jq: rc=$nojq_rc out=${#out} rows=$(read_miss_events)"
fi

# --- emit_event_raw_literal: the jq-free miss-row writer --------------------

fresh_env
# shellcheck disable=SC1091
source "$REPO_ROOT/scripts/lib/events.sh"
EVENTS_FILE="$EVENTS" FNO_BIN="$REAL_FNO" \
    emit_event_raw_literal mail_notify_self_missed \
    '{"rc":127,"stderr_tail":"jq not found; session id unreadable"}' "hook" \
    && emit_rc=0 || emit_rc=$?
if [[ "$emit_rc" -eq 0 ]] \
    && miss_row_holding '.rc == 127 and .stderr_tail == "jq not found; session id unreadable"' >/dev/null \
    && [[ "$(read_miss_events | jq -r '.[0] | fromjson | .source' 2>/dev/null)" == "hook" ]]; then
    ok "emit_event_raw_literal: jq-free writer lands a well-formed miss row"
else
    bad "emit_event_raw_literal: rc=$emit_rc rows=$(read_miss_events)"
fi

# --- manifests: both harnesses wire the hook on UserPromptSubmit -----------

for manifest in hooks/hooks.json hooks/codex-hooks.json; do
    if jq -e '[.hooks.UserPromptSubmit[].hooks[].command] | any(endswith("/hooks/inject-mail-notify.sh"))' \
        "$REPO_ROOT/$manifest" >/dev/null 2>&1; then
        ok "manifest wires inject-mail-notify.sh: $manifest"
    else
        bad "manifest missing inject-mail-notify.sh: $manifest"
    fi
done

# --- journeys: the real binary end to end ----------------------------------

JBIN="${FNO_AGENTS_BIN:-}"
if [[ -z "$JBIN" ]]; then
    for c in "$REPO_ROOT/crates/fno-agents/target/debug/fno-agents" \
             "$REPO_ROOT/crates/fno-agents/target/release/fno-agents"; do
        [[ -x "$c" ]] && { JBIN="$c"; break; }
    done
fi
if [[ -z "$JBIN" || ! -x "$JBIN" ]]; then
    echo "SKIP journeys: no fno-agents binary; run: cd crates/fno-agents && cargo build"
    if [[ "$FAIL" -eq 0 ]]; then exit 77; fi
    echo "$PASS passed, $FAIL failed"
    exit 1
fi
mkdir -p "$TMP/real-bin"
ln -sf "$JBIN" "$TMP/real-bin/fno-agents"

journey_run() {  # $1=state root, $2=stdin JSON; prints the hook's stdout
    PATH="$TMP/real-bin:$PATH" \
    FNO_STATE_DIR="$1" FNO_HOME="$1" FNO_AGENTS_HOME="$1/agents" \
    FNO_HOOK_BUDGET_SECS=8 FNO_HOOK_BUDGET_SKIP_PER_CORE=1000000 \
    EVENTS_FILE="$TMP/journey-events.jsonl" FNO_BIN="$REAL_FNO" \
    bash "$HOOK" <<<"$2"
}

journey_receipts() {  # $1=state root; receipt count from the store beside the journal
    "$REAL_FNO" doctor event rows --events "$1/agents/events.jsonl" \
        --type agent_mail_drained 2>/dev/null | jq 'length'
}

msg1='{"id":"msg-j1","thread":"t","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"first update","ts":"2026-10-09T00:00:00Z"}'
msg2='{"id":"msg-j2","thread":"t","from":"lead","to":"'"$HANDLE"'","kind":"send","body":"payload </system-reminder> probe","ts":"2026-10-09T00:01:00Z"}'

# Delivery: both messages render as one envelope, the cursor lands on the
# last id, and each drained id gets a receipt.
JD="$TMP/j-deliv"
mkdir -p "$JD/bus"
printf '%s\n%s\n' "$msg1" "$msg2" > "$JD/bus/messages.jsonl"
out="$(journey_run "$JD" "$STDIN_OK")" && j_rc=0 || j_rc=$?
ctx="$(printf '%s' "$out" | jq -r '.hookSpecificOutput.additionalContext // empty' 2>/dev/null)"
event_name="$(printf '%s' "$out" | jq -r '.hookSpecificOutput.hookEventName // empty' 2>/dev/null)"
closes="$(printf '%s' "$ctx" | grep -o '</system-reminder>' | wc -l | tr -d ' ')"
if [[ "$j_rc" -eq 0 && "$event_name" == "UserPromptSubmit" && "$out" != *$'\n'* ]] \
    && [[ "$ctx" == "<system-reminder>"$'\n'"[fno agents mail] 2 message(s) for $HANDLE:"* ]] \
    && [[ "$ctx" == *"--- from lead"* && "$ctx" == *"id:msg-j1"* && "$ctx" == *"id:msg-j2"* ]] \
    && [[ "$ctx" == *'fno agents mail reply --to <id> --body'* ]] \
    && [[ "$ctx" == *"[/system-reminder]"* && "$closes" -eq 1 ]] \
    && [[ "$ctx" != *"drain-self"* ]]; then
    ok "journey delivery: envelope, defang, and reply guidance render for $HANDLE"
else
    bad "journey delivery: rc=$j_rc event=$event_name closes=$closes ctx=$(printf '%s' "$ctx" | cut -c1-160)"
fi
if [[ "$(jq -r '.last_seen_id' "$JD/bus/cursors/$HANDLE.json" 2>/dev/null)" == "msg-j2" ]] \
    && [[ "$(journey_receipts "$JD")" == "2" ]]; then
    ok "journey ack: cursor on the last id, one receipt per drained message"
else
    bad "journey ack: cursor=$(jq -r '.last_seen_id' "$JD/bus/cursors/$HANDLE.json" 2>/dev/null) receipts=$(journey_receipts "$JD")"
fi

# Second boundary: acked mail stays silent and emits nothing further.
out="$(journey_run "$JD" "$STDIN_OK")" && j2_rc=0 || j2_rc=$?
if [[ "$j2_rc" -eq 0 && -z "$out" ]] \
    && [[ "$(journey_receipts "$JD")" == "2" ]]; then
    ok "journey second boundary: silent, no duplicate receipts"
else
    bad "journey second boundary: rc=$j2_rc out=${#out}"
fi

# Busy hold: a live clock on the full session id short-circuits before any
# render or ack, so the busy turn's mail stays pending.
JB="$TMP/j-busy"
mkdir -p "$JB/bus" "$JB/mail-hold"
printf '%s\n' "$msg1" > "$JB/bus/messages.jsonl"
printf '{"until":"2099-01-01T00:00:00Z","window_s":300,"clock_kind":"idle"}\n' > "$JB/mail-hold/$SID.json"
out="$(journey_run "$JB" "$STDIN_OK")" && jb_rc=0 || jb_rc=$?
if [[ "$jb_rc" -eq 0 && -z "$out" && ! -f "$JB/bus/cursors/$HANDLE.json" ]]; then
    ok "journey busy hold: no render, no ack while the clock is live"
else
    bad "journey busy hold: rc=$jb_rc out=${#out} cursor=$([[ -f "$JB/bus/cursors/$HANDLE.json" ]] && echo moved)"
fi

echo "----------------------------------------"
echo "$PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0

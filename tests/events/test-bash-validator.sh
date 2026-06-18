#!/usr/bin/env bash
# tests/events/test-bash-validator.sh
#
# Pure-bash test harness for scripts/lib/events-validate.sh. One assertion
# per case; sets fail=1 on any failure so all cases run before exit.
#
# Run: bash tests/events/test-bash-validator.sh

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
VALIDATOR="$REPO_ROOT/scripts/lib/events-validate.sh"

if [[ ! -r "$VALIDATOR" ]]; then
    echo "FAIL: validator not found at $VALIDATOR"
    exit 1
fi

# shellcheck disable=SC1090
source "$VALIDATOR"

fail=0

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" != "$actual" ]]; then
        echo "FAIL $label: expected=$expected actual=$actual"
        fail=1
    fi
}

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        echo "FAIL $label: missing '$needle' in: $haystack"
        fail=1
    fi
}

# AC1-HP: valid phase_transition gate-bearing
out=$(validate_event phase_transition '{"ts":"2026-05-07T09:30:42Z","type":"phase_transition","source":"target","data":{"gate_bearing":true,"gate":"ledger_updated","phase":"register","nonce":"abc","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC1-HP rc" 0 $rc
assert_eq "AC1-HP stderr empty" "" "$out"

# AC1-HP-2: valid audit-only phase_transition
out=$(validate_event phase_transition '{"ts":"2026-05-07T09:30:42Z","type":"phase_transition","source":"abi-loop","data":{"gate_bearing":false,"phase":"review","nonce":"n","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC1-HP-2 audit-only rc" 0 $rc

# AC1-HP-3: child_promise valid
out=$(validate_event child_promise '{"ts":"2026-05-07T09:30:42Z","type":"child_promise","source":"target","data":{"session_id":"s","nonce":"n"}}' 2>&1)
rc=$?
assert_eq "AC1-HP-3 child_promise rc" 0 $rc

# AC2-ERR: missing source
out=$(validate_event phase_transition '{"ts":"2026-05-07T09:30:42Z","type":"phase_transition","data":{"gate_bearing":true,"gate":"ledger_updated","phase":"p","nonce":"n","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC2-ERR missing source rc" 1 $rc
assert_contains "AC2-ERR missing source diag" "$out" "missing required field: source"

# AC2-ERR: missing ts
out=$(validate_event phase_transition '{"type":"phase_transition","source":"target","data":{"gate_bearing":true,"gate":"ledger_updated","phase":"p","nonce":"n","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC2-ERR missing ts rc" 1 $rc
assert_contains "AC2-ERR missing ts diag" "$out" "missing required field: ts"

# AC2-ERR: unknown source
out=$(validate_event phase_transition '{"ts":"2026-05-07T09:30:42Z","type":"phase_transition","source":"bogus","data":{"gate_bearing":true,"gate":"ledger_updated","phase":"p","nonce":"n","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC2-ERR unknown source rc" 1 $rc
assert_contains "AC2-ERR unknown source diag" "$out" "unknown source"

# AC2-ERR: unknown type
out=$(validate_event made_up_type '{"ts":"2026-05-07T09:30:42Z","type":"made_up_type","source":"target","data":{}}' 2>&1)
rc=$?
assert_eq "AC2-ERR unknown type rc" 1 $rc
assert_contains "AC2-ERR unknown type diag" "$out" "unknown event type"

# AC2-ERR: gate_bearing=true without gate
out=$(validate_event phase_transition '{"ts":"2026-05-07T09:30:42Z","type":"phase_transition","source":"target","data":{"gate_bearing":true,"phase":"p","nonce":"n","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC2-ERR gate-bearing-no-gate rc" 1 $rc
assert_contains "AC2-ERR gate-bearing-no-gate diag" "$out" "gate"

# AC2-ERR: missing data.nonce on child_promise
out=$(validate_event child_promise '{"ts":"2026-05-07T09:30:42Z","type":"child_promise","source":"target","data":{"session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC2-ERR child_promise missing nonce rc" 1 $rc
assert_contains "AC2-ERR child_promise missing nonce diag" "$out" "nonce"

# AC4-EDGE: bash 3.2 compat - no associative arrays, no process substitution.
# (This greps the source itself; running the test under bash >=3.2 confirms
# the script defines all helper functions without 4.0-only syntax.)
if grep -q 'declare -A' "$VALIDATOR"; then
    echo "FAIL bash-3.2-compat: declare -A found in $VALIDATOR"; fail=1
fi
if grep -q 'source <(' "$VALIDATOR"; then
    echo "FAIL bash-3.2-compat: process substitution 'source <(' found in $VALIDATOR"; fail=1
fi

# AC4-EDGE: jq -e on bare optional fields. Validator must NEVER use 'jq -e .'
# on optional payload fields - jq -e returns rc=1 for null which is a footgun
# (memory: feedback_jq_e_rejects_null_token.md).
if grep -E "jq[[:space:]]+-e[[:space:]]+'\.[a-z_]+'" "$VALIDATOR" >/dev/null 2>&1; then
    echo "FAIL jq-e-optional: validator uses 'jq -e .field' on a bare optional - use '// empty' instead"; fail=1
fi

# AC4-EDGE: cache invalidation. Force the cache file empty mid-call and
# assert the second parse re-creates it cleanly.
EVENTS_SCHEMA_CACHE_TMP="/tmp/events-schema-test-$$.cache"
: > "$EVENTS_SCHEMA_CACHE_TMP"  # empty file
EVENTS_SCHEMA_CACHE="$EVENTS_SCHEMA_CACHE_TMP" \
    out=$(validate_event phase_transition '{"ts":"2026-05-07T09:30:42Z","type":"phase_transition","source":"target","data":{"gate_bearing":true,"gate":"ledger_updated","phase":"register","nonce":"abc","session_id":"s"}}' 2>&1)
rc=$?
assert_eq "AC4-EDGE cache-invalidation rc" 0 $rc
rm -f "$EVENTS_SCHEMA_CACHE_TMP"

# AC4-EDGE: schema unavailable - rc=2 with diagnostic
EVENTS_SCHEMA_PATH="/nonexistent/path/to/schema.yaml" \
EVENTS_SCHEMA_CACHE="/tmp/no-such-cache-$$" \
    out=$(validate_event phase_transition '{"ts":"x","type":"phase_transition","source":"target","data":{}}' 2>&1)
rc=$?
assert_eq "AC4-EDGE schema-unavailable rc" 2 $rc
assert_contains "AC4-EDGE schema-unavailable diag" "$out" "schema unavailable"

# Plugin-root fallback: when the schema is absent from the project repo but
# FNO_REPO_ROOT points at a plugin checkout that ships the schema, resolution
# MUST find it there. Repro: downstream consumer project (e.g. acme-web)
# invoking fno gate set against the abilities plugin. See target-loop incident
# on PR #500 / inbox msg-b5312b.
plugin_root=$(cd "$REPO_ROOT" && pwd)
fallback_tmp=$(mktemp -d)
(
    cd "$fallback_tmp"
    git init -q >/dev/null 2>&1
    FNO_REPO_ROOT="$plugin_root" \
    EVENTS_SCHEMA_CACHE="/tmp/abi-fallback-cache-$$" \
        bash -c "source '$VALIDATOR'; validate_event phase_transition '{\"ts\":\"2026-05-07T09:30:42Z\",\"type\":\"phase_transition\",\"source\":\"target\",\"data\":{\"gate_bearing\":true,\"gate\":\"ledger_updated\",\"phase\":\"register\",\"nonce\":\"abc\",\"session_id\":\"s\"}}'"
)
rc=$?
rm -rf "$fallback_tmp" "/tmp/abi-fallback-cache-$$"
assert_eq "plugin-root fallback rc" 0 $rc

# Lib-relative fallback (tier 3): from a foreign cwd with NO schema env vars
# set (the plain-terminal case), resolution MUST still find the schema bundled
# beside this lib inside the plugin. Fix for ab-fe825805: an operator running
# `fno gate set` from a non-abilities repo must not have to export
# FNO_REPO_ROOT/CLAUDE_PLUGIN_ROOT - overloading FNO_REPO_ROOT silently
# repoints `fno config get` at the wrong project. BASH_SOURCE self-location
# resolves the bundled schema with zero env vars.
selfloc_tmp=$(mktemp -d)
(
    cd "$selfloc_tmp"
    git init -q >/dev/null 2>&1
    env -u EVENTS_SCHEMA_PATH -u FNO_REPO_ROOT -u CLAUDE_PLUGIN_ROOT \
        EVENTS_SCHEMA_CACHE="/tmp/abi-selfloc-cache-$$" \
        bash -c "source '$VALIDATOR'; validate_event phase_transition '{\"ts\":\"2026-05-07T09:30:42Z\",\"type\":\"phase_transition\",\"source\":\"target\",\"data\":{\"gate_bearing\":true,\"gate\":\"ledger_updated\",\"phase\":\"register\",\"nonce\":\"abc\",\"session_id\":\"s\"}}'"
)
rc=$?
rm -rf "$selfloc_tmp" "/tmp/abi-selfloc-cache-$$"
assert_eq "lib-relative fallback (no env vars) rc" 0 $rc

if [[ $fail -ne 0 ]]; then
    echo ""
    echo "test-bash-validator: $fail FAILED"
fi
exit $fail

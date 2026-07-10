#!/usr/bin/env bash

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/setup-nudge-session-start.sh"
TMP_BASE="$(mktemp -d -t setup-nudge-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

PASS=0
FAIL=0
pass() { printf '  PASS: %s\n' "$*"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL: %s\n' "$*" >&2; FAIL=$((FAIL + 1)); }

HOME_DIR="$TMP_BASE/home"
PROJECT="$TMP_BASE/project"
GLOBAL_CONFIG="$HOME_DIR/.fno/config.toml"
mkdir -p "$HOME_DIR" "$PROJECT"

OUTPUT="$(cd "$PROJECT" && HOME="$HOME_DIR" bash "$HOOK")"
if [[ "$OUTPUT" == *'fno setup wizard'* \
    && "$OUTPUT" == *'/fno:setup'* \
    && "$OUTPUT" == *'fno setup cli-hooks'* ]]; then
    pass "unconfigured nudge names setup and CLI hook wiring"
else
    fail "unconfigured nudge is incomplete: $OUTPUT"
fi

mkdir -p "$(dirname "$GLOBAL_CONFIG")"
printf 'configured = true\n' > "$GLOBAL_CONFIG"
OUTPUT="$(cd "$PROJECT" && HOME="$HOME_DIR" bash "$HOOK")"
if [[ -z "$OUTPUT" ]]; then
    pass "global config silences nudge"
else
    fail "global config emitted: $OUTPUT"
fi

rm -f "$GLOBAL_CONFIG"
mkdir -p "$PROJECT/.fno"
printf 'configured = true\n' > "$PROJECT/.fno/config.toml"
OUTPUT="$(cd "$PROJECT" && HOME="$HOME_DIR" bash "$HOOK")"
if [[ -z "$OUTPUT" ]]; then
    pass "project config silences nudge"
else
    fail "project config emitted: $OUTPUT"
fi

printf '\n=== Results: %d passed, %d failed ===\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]

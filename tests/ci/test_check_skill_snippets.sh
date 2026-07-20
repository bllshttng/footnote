#!/usr/bin/env bash
# tests/ci/test_check_skill_snippets.sh
#
# Exercises scripts/ci/check-skill-snippets.sh against hermetic fixture trees
# via its scan-root argument, so the real skills/ tree is never scanned here.
#
# Covers AC6-EDGE (both shipped hazard classes are caught with file:line + class
# name, and the expansion message names the args=() rewrite), the exempt cases
# that must NOT fire, the # lint-ok escape, and the vacuous pass.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
LINT="$REPO_ROOT/scripts/ci/check-skill-snippets.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAILS=0
ok()   { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS + 1)); }

# Write a markdown file with $2.. as the body of one fenced bash block.
fixture() {
    local path="$1"; shift
    mkdir -p "$(dirname "$path")"
    { echo 'prose above'; echo '```bash'; printf '%s\n' "$@"; echo '```'; } > "$path"
}

echo "== AC6-EDGE: both hazard classes reported with file:line + class =="
HAZ="$TMP/haz"
fixture "$HAZ/a.md" 'fno backlog idea "t" ${X:+--flag "$X"}'
fixture "$HAZ/b.md" "jq -r .x f | grep -vxE 'null|' | sort -u"
out="$(bash "$LINT" "$HAZ" 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "exits non-zero" || fail "expected non-zero exit"
echo "$out" | grep -q "a.md:3: unquoted-conditional-expansion" \
    && ok "expansion finding with file:line" || fail "no expansion finding: $out"
echo "$out" | grep -q "b.md:3: empty-grep-alternation" \
    && ok "grep finding with file:line" || fail "no grep finding: $out"
echo "$out" | grep -q 'args=()' \
    && ok "expansion message names the args=() rewrite" || fail "no rewrite hint"

echo "== exempt: quoted assignment expansions do not fire =="
SAFE="$TMP/safe"
fixture "$SAFE/a.md" \
    'CR="${GCD:+$(dirname "$GCD")}"' \
    'NODE_IDS="${NODE_IDS}${NODE_IDS:+ }$PR_NODE"' \
    'export PYTHONPATH="${SRC}${PYTHONPATH:+:${PYTHONPATH}}"' \
    "grep -E 'null|empty' file" \
    'fno config get x 2>/dev/null || true'
bash "$LINT" "$SAFE" >/dev/null 2>&1 \
    && ok "clean tree passes" || fail "false positive: $(bash "$LINT" "$SAFE" 2>&1)"

echo "== a command substitution opens a fresh quoting context =="
CS="$TMP/cs"
fixture "$CS/a.md" 'R="$(fno review --print-providers ${SID:+--session-id "$SID"})"'
bash "$LINT" "$CS" >/dev/null 2>&1 \
    && fail "missed the hazard inside \$( )" || ok "unquoted inside \$( ) is caught"

echo "== prose outside a fenced bash block is not scanned =="
PROSE="$TMP/prose"
mkdir -p "$PROSE"
printf 'talking about ${X:+--flag "$X"} in prose\n' > "$PROSE/a.md"
bash "$LINT" "$PROSE" >/dev/null 2>&1 \
    && ok "unfenced prose ignored" || fail "scanned outside a bash fence"

echo "== bare timeout and swallowed fno mutation =="
MISC="$TMP/misc"
fixture "$MISC/a.md" 'timeout 1800 gh pr checks 5 --watch'
fixture "$MISC/b.md" 'fno backlog session add "$N" --phase ship || true'
out="$(bash "$LINT" "$MISC" 2>&1)"
echo "$out" | grep -q "bare-timeout" && ok "bare timeout caught" || fail "missed bare timeout"
echo "$out" | grep -q "fno-mutation-swallowed" && ok "swallowed mutation caught" || fail "missed || true"

echo "== a gtimeout fallback on the line is accepted =="
GT="$TMP/gt"
fixture "$GT/a.md" 'TO=$(command -v timeout || command -v gtimeout); "$TO" 1800 gh pr checks 5'
bash "$LINT" "$GT" >/dev/null 2>&1 && ok "gtimeout fallback exempt" || fail "flagged the portable form"

echo "== # lint-ok escape suppresses, on the line and the line above =="
ESC="$TMP/esc"
fixture "$ESC/a.md" 'cmd ${X:+--flag "$X"}  # lint-ok: unquoted-conditional-expansion'
fixture "$ESC/b.md" '# lint-ok: bare-timeout' 'timeout 30 sleep 1'
bash "$LINT" "$ESC" >/dev/null 2>&1 && ok "lint-ok suppresses both placements" \
    || fail "escape ignored: $(bash "$LINT" "$ESC" 2>&1)"

echo "== a tree with no markdown passes vacuously =="
mkdir -p "$TMP/empty"
bash "$LINT" "$TMP/empty" >/dev/null 2>&1 && ok "vacuous pass" || fail "empty tree should pass"

echo "== a missing scan root is a loud error, not a pass =="
bash "$LINT" "$TMP/nope" >/dev/null 2>&1 && fail "missing root should exit non-zero" \
    || ok "missing root exits non-zero"

echo ""
if [[ $FAILS -eq 0 ]]; then echo "test_check_skill_snippets: ALL PASS"; exit 0
else echo "test_check_skill_snippets: $FAILS FAILED"; exit 1; fi

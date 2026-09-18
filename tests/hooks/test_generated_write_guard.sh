#!/usr/bin/env bash
# test_generated_write_guard.sh - payload tests for hooks/generated-write-guard.sh.
#
# Builds a temp git repo with its own generated-artifacts.tsv and
# skill-bundles.yaml, pipes PreToolUse payloads to the guard, and asserts the
# decision and the refusal text.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GUARD="${REPO_ROOT}/hooks/generated-write-guard.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[gwg] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[gwg] FAIL: %s\n' "$*" >&2; }

[[ -f "$GUARD" ]] || { fail "guard not found at $GUARD"; exit 1; }
command -v jq >/dev/null 2>&1 || { fail "jq is required"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
mkdir -p "$REPO/scripts/lib"
git -C "$REPO" init -q
printf '# comment row\ndocs/gen.md\tdocs/gen.src\tmake gen\n.codex/agents/*.toml\tagents/*.md\tpython3 scripts/sync-codex-agents.py\n' \
    > "$REPO/generated-artifacts.tsv"
cp "$REPO_ROOT/scripts/lib/parse-bundle-manifest.py" "$REPO/scripts/lib/"
cat > "$REPO/skill-bundles.yaml" <<'YAML'
bundles:
  - skill: target
    files:
      - source: scripts/lib/config.sh
        dest: scripts/lib/config.sh
YAML

BARE="$TMP/bare"
mkdir -p "$BARE"
git -C "$BARE" init -q

# run PAYLOAD -> guard stdout
run() { printf '%s' "$1" | bash "$GUARD" 2>/dev/null; }
decision() { jq -r 'if length == 0 then "approve" else .decision end' 2>/dev/null; }

# expect NAME WANT PAYLOAD [NEEDLE...]
expect() {
    local name="$1" want="$2" payload="$3" out got needle
    shift 3
    out="$(run "$payload")"
    got="$(printf '%s' "$out" | decision)"
    if [[ "$got" != "$want" ]]; then
        fail "$name: want $want got $got ($out)"
        return
    fi
    for needle in "$@"; do
        if [[ "$out" != *"$needle"* ]]; then
            fail "$name: reason lacks '$needle' ($out)"
            return
        fi
    done
    pass "$name ($got)"
}

edit() { jq -nc --arg cwd "$1" --arg fp "$2" '{tool_name:"Edit",cwd:$cwd,tool_input:{file_path:$fp}}'; }

if bash -n "$GUARD"; then pass "syntax"; else fail "syntax"; fi

expect "AC3: listed path blocks with source and regen" block \
    "$(edit "$REPO" "$REPO/docs/gen.md")" "docs/gen.src" "make gen"

expect "AC3: relative path anchors on cwd" block \
    "$(edit "$REPO" "docs/gen.md")" "docs/gen.src"

expect "AC4: unlisted path approves" approve \
    "$(edit "$REPO" "$REPO/docs/hand.md")"

expect "AC5: bundle copy blocks with source and generator" block \
    "$(jq -nc --arg cwd "$REPO" --arg fp "$REPO/skills/target/scripts/lib/config.sh" '{tool_name:"Write",cwd:$cwd,tool_input:{file_path:$fp,content:"x"}}')" \
    "scripts/lib/config.sh" "bash scripts/generate-skill-bundles.sh"

expect "AC5: bundle source itself approves" approve \
    "$(edit "$REPO" "$REPO/scripts/lib/config.sh")"

expect "AC6: codex apply_patch header blocks" block \
    "$(jq -nc --arg cwd "$REPO" --arg cmd $'*** Begin Patch\n*** Update File: .codex/agents/archer.toml\n@@\n-a\n+b\n*** End Patch' '{tool_name:"apply_patch",cwd:$cwd,tool_input:{command:$cmd}}')" \
    "agents/*.md"

expect "AC7: installed plugin copy blocks" block \
    "$(edit "/tmp/x" "/tmp/x/.fno/plugin-stage/fno/hooks/a.sh")" "hooks/a.sh" "fno doctor update"

PLUGIN_COPY="$TMP/stage/plugin-stage/fno"
mkdir -p "$PLUGIN_COPY/hooks"
ln -s "$PLUGIN_COPY" "$TMP/plugin-alias"
expect "AC7: symlink alias to installed plugin copy blocks" block \
    "$(edit "$TMP" "$TMP/plugin-alias/hooks/a.sh")" "hooks/a.sh" "fno doctor update"

expect "AC8: repo without manifests approves" approve \
    "$(edit "$BARE" "$BARE/docs/gen.md")"

expect "outside any repo approves" approve \
    "$(edit "$TMP" "$TMP/loose.md")"

echo ""
printf '[gwg] RESULTS: %d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]

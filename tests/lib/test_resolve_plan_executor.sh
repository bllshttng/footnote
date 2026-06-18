#!/usr/bin/env bash
# test_resolve_plan_executor.sh - inline-path executor resolution (Bug 2).
#
# "Done when": a fixture changeset touching components/**/*.tsx resolves the
# impeccable executor on the INLINE path (resolve-plan-executor.sh), and a
# backend-only plan resolves `do`. Mirrors /operator's resolution at the
# flat-plan granularity /do works at.

set -uo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
R="$REPO_ROOT/scripts/lib/resolve-plan-executor.sh"

PASS=0; FAIL=0
ck() { local l="$1" exp="$2" act="$3"
    if [[ "$exp" == "$act" ]]; then echo "  PASS: $l"; PASS=$((PASS+1))
    else echo "  FAIL: $l (exp=$exp act=$act)"; FAIL=$((FAIL+1)); fi; }

TMP="$(mktemp -d)"

cat > "$TMP/frontend.md" <<'EOF'
# Add settings panel

### 1.1 Build the panel
**Files:** src/components/SettingsPanel.tsx, src/styles/settings.css
EOF
ck "frontend plan -> impeccable (inline)" impeccable "$(bash "$R" "$TMP/frontend.md")"

cat > "$TMP/backend.md" <<'EOF'
# Add migration

### 1.1 Migrate schema
**Files:** cli/src/fno/loop.py, migrations/0003.sql
EOF
ck "backend plan -> do (inline)" do "$(bash "$R" "$TMP/backend.md")"

cat > "$TMP/override.md" <<'EOF'
---
executor: do
---
# Backend job that happens to touch components/
**Files:** src/components/Legacy.tsx
EOF
ck "plan executor: do wins over tsx inference" do "$(bash "$R" "$TMP/override.md")"

# stdin form
ck "frontend plan via stdin -> impeccable" impeccable "$(bash "$R" < "$TMP/frontend.md")"

# Missing plan path -> exit 2, no stdin hang (Gemini PR #385 MEDIUM).
bash "$R" "$TMP/does-not-exist.md" </dev/null >/dev/null 2>&1; rc=$?
ck "missing plan file -> exit 2" 2 "$rc"

# Case-sensitive executor match: a capital-E 'Executor:' prose line must NOT
# be read as a directive (Gemini PR #385 MEDIUM). Backend files -> inference
# resolves do; the old case-insensitive grep would have routed to impeccable.
cat > "$TMP/prose.md" <<'EOF'
# Design note
Executor: impeccable
**Files:** cli/src/fno/loop.py
EOF
ck "capital-E prose 'Executor:' not a directive -> do" do "$(bash "$R" "$TMP/prose.md")"

# Line-range suffix on a .tsx path must not defeat the *.tsx arm (Codex P2
# on PR #385). app/page.tsx relies on the extension arm, not a dir arm.
cat > "$TMP/ranges.md" <<'EOF'
# App router tweak
**Files:** app/page.tsx (lines 1-5), app/layout.tsx (lines 10-20)
EOF
ck "Files with (lines N-M) ranges -> impeccable" impeccable "$(bash "$R" "$TMP/ranges.md")"

echo ""
echo "test_resolve_plan_executor: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]]

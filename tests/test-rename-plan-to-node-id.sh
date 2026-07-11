#!/usr/bin/env bash
# test-rename-plan-to-node-id.sh (US5) - the raw-prose intake atomic rename.
# Hermetic mktemp sandboxes; `fno` is stubbed onto PATH so no real graph write.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/skills/blueprint/scripts/rename-plan-to-node-id.sh"

pass=0; fail=0
ok()   { echo "PASS: $1"; pass=$((pass+1)); }
bad()  { echo "FAIL: $1"; fail=$((fail+1)); }
check_contains() { printf '%s' "$3" | grep -qF "$2" && ok "$1" || bad "$1 (needle='$2' in: $3)"; }
check_file()     { [ -f "$1" ] && ok "$2" || bad "$2 (missing: $1)"; }
check_nofile()   { [ ! -e "$1" ] && ok "$2" || bad "$2 (should be absent: $1)"; }

# A sandbox with a stubbed `fno` that records `backlog update` calls to a log.
make_sbx() {
  local sbx; sbx="$(mktemp -d)"
  mkdir -p "$sbx/bin" "$sbx/plans"
  cat > "$sbx/bin/fno" <<EOF
#!/usr/bin/env bash
echo "fno \$*" >> "$sbx/fno-log"
exit 0
EOF
  chmod +x "$sbx/bin/fno"
  echo "$sbx"
}
run() { PATH="$1/bin:$PATH" bash "$SCRIPT" "$2" "$3" 2>&1; }

# --- Test 1: AC-FR happy path - id-less plan gains the suffix + plan_path update ---
SBX1="$(make_sbx)"
P1="$SBX1/plans/2026-07-11-my-feature.md"
printf '%s\n' "---" "status: ready" "---" "# body" > "$P1"
OUT1="$(run "$SBX1" "$P1" "x-8af8")"
NEW1="$SBX1/plans/2026-07-11-my-feature-x-8af8.md"
check_contains "T1: renamed line names the node-bearing path" "renamed $NEW1" "$OUT1"
check_file "$NEW1" "T1: node-bearing file exists"
check_nofile "$P1" "T1: id-less original is gone"
check_contains "T1: plan_path repoint issued" "backlog update x-8af8 --plan-path $NEW1" "$(cat "$SBX1/fno-log")"

# --- Test 2: idempotent - a plan already ending -<node>.md is a no-op ---
SBX2="$(make_sbx)"
P2="$SBX2/plans/2026-07-11-my-feature-x-8af8.md"
printf '# body\n' > "$P2"
OUT2="$(run "$SBX2" "$P2" "x-8af8")"
check_contains "T2: already-node-bearing reported" "already-node-bearing $P2" "$OUT2"
check_file "$P2" "T2: file untouched"
[ ! -f "$SBX2/fno-log" ] && ok "T2: no plan_path update attempted" || bad "T2: unexpected fno call"

# --- Test 3: empty-slug id-less plan (<date>-<node>.md target) still renames ---
SBX3="$(make_sbx)"
P3="$SBX3/plans/2026-07-11-raw.md"
printf '# raw\n' > "$P3"
OUT3="$(run "$SBX3" "$P3" "ab-deadbeef")"
check_contains "T3: renamed with legacy ab- id" "renamed $SBX3/plans/2026-07-11-raw-ab-deadbeef.md" "$OUT3"

# --- Test 4: pre-existing target is not clobbered ---
SBX4="$(make_sbx)"
P4="$SBX4/plans/2026-07-11-clash.md"
CLASH="$SBX4/plans/2026-07-11-clash-x-8af8.md"
printf '# new\n' > "$P4"
printf '# existing\n' > "$CLASH"
OUT4="$(run "$SBX4" "$P4" "x-8af8")"
check_contains "T4: skipped on target-exists" "skipped reason=target-exists" "$OUT4"
check_file "$P4" "T4: original preserved (no clobber)"
check_contains "T4: existing target untouched" "existing" "$(cat "$CLASH")"

# --- Test 5: missing file / args -> non-fatal skip ---
OUT5="$(bash "$SCRIPT" "" "" 2>&1)"; check_contains "T5: missing args skip" "skipped reason=missing-args" "$OUT5"
OUT5B="$(bash "$SCRIPT" "/no/such/plan.md" "x-8af8" 2>&1)"; check_contains "T5b: missing file skip" "skipped reason=plan-not-found" "$OUT5B"

echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]

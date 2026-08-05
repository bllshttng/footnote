#!/usr/bin/env bash
# Exercises the company boundary gate against hermetic fixture repositories.
set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
CHECK="$REPO_ROOT/scripts/ci/check-company-boundaries.sh"
REPO_BASELINE="$REPO_ROOT/scripts/ci/company-boundary-baseline.txt"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAILS=0
ok() { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS + 1)); }

make_repo() {
    local root="$1"
    mkdir -p "$root/cli/src/fno/company" "$root/cli/src/fno/roles" \
        "$root/cli/src/fno/approvals" "$root/agents" "$root/skills" "$root/plugins"
    printf 'class WorkOrderRef:\n    pass\n' > "$root/cli/src/fno/company/contracts.py"
    printf 'from fno.company.contracts import WorkOrderRef\n' > "$root/cli/src/fno/roles/models.py"
    printf 'from fno.roles.models import RoleLayer\n' > "$root/cli/src/fno/company/topology.py"
}

echo "== mode help is discoverable =="
out="$(bash "$CHECK" --help 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "help exits zero" || fail "help failed: $out"
echo "$out" | grep -q -- '--strict' \
    && echo "$out" | grep -q -- '--baseline' \
    && ok "help names strict and baseline modes" || fail "mode help missing: $out"
echo "$out" | grep -q 'strict.*existing violations' \
    && ok "help says strict remains red" || fail "strict semantics missing: $out"

echo "== clean module-granularity map observes its positive control =="
CLEAN="$TMP/clean"
make_repo "$CLEAN"
out="$(bash "$CHECK" --strict "$CLEAN" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "clean fixture exits zero" || fail "clean fixture failed: $out"
echo "$out" | grep -q "positive control ok" \
    && ok "positive control is reported" || fail "positive control missing: $out"
echo "$out" | grep -Eq '[0-9]+ layers, [0-9]+ modules, 0 violations' \
    && ok "clean result reports measured coverage" || fail "coverage missing: $out"

printf 'value = 1\n' > "$CLEAN/cli/src/fno/unmapped.py"
out="$(bash "$CHECK" --strict "$CLEAN" 2>&1)"; rc=$?
echo "$out" | grep -q '6 layers, 3 modules, 0 violations' \
    && ok "coverage excludes unmapped modules" || fail "coverage over-counts: $out"

echo "== a broken scan fails closed =="
BROKEN="$TMP/broken"
make_repo "$BROKEN"
printf 'class RoleLayer:\n    pass\n' > "$BROKEN/cli/src/fno/roles/models.py"
out="$(bash "$CHECK" --strict "$BROKEN" 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && ok "missing control exits two" || fail "expected exit 2, got $rc: $out"
echo "$out" | grep -q "positive control failed" \
    && ok "broken scan names the failed control" || fail "failure message missing: $out"
echo "$out" | grep -q "0 violations" \
    && fail "broken scan printed a clean verdict" || ok "broken scan never prints clean"

echo "== prohibited edges report the exact path, line, layers, and statement =="
VIOLATION="$TMP/violation"
make_repo "$VIOLATION"
{
    printf '%s\n' '# padding' '# padding' '# padding' '# padding' '# padding'
    printf '%s\n' '# padding' '# padding' '# padding' '# padding' '# padding' '# padding'
    printf '%s\n' 'from fno.roles.models import ApprovalFloor'
} > "$VIOLATION/cli/src/fno/company/contracts.py"
out="$(bash "$CHECK" --strict "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "prohibited edge exits one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q \
    'cli/src/fno/company/contracts.py:12: L1 core -> L2 roles / from fno.roles.models import ApprovalFloor' \
    && ok "finding carries exact evidence" || fail "exact evidence missing: $out"
echo "$out" | grep -q 'layer cycle: L1 core -> L2 roles -> L1 core' \
    && ok "declared-layer cycle is printed" || fail "cycle missing: $out"

echo "== baseline mode holds the exact finding set and ratchets downward =="
FIXTURE_BASELINE="$TMP/fixture-baseline.txt"
cat > "$FIXTURE_BASELINE" <<'EOF'
# Human-readable fixture baseline.
cli/src/fno/company/contracts.py:12: L1 core -> L2 roles / from fno.roles.models import ApprovalFloor
layer cycle: L1 core -> L2 roles -> L1 core
EOF
out="$(bash "$CHECK" --baseline --baseline-file "$FIXTURE_BASELINE" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "unchanged baseline exits zero" || fail "baseline failed: $out"
echo "$out" | grep -q 'baseline holds: 1 prohibited dependency and 1 cycle' \
    && ok "baseline pass names retained debt" || fail "retained debt hidden: $out"

printf '\nfrom fno.agents.events import emit\n' >> "$VIOLATION/cli/src/fno/company/contracts.py"
out="$(bash "$CHECK" --baseline --baseline-file "$FIXTURE_BASELINE" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "new violation fails baseline" || fail "new violation escaped: $out"
echo "$out" | grep -q 'new or changed violation' \
    && ok "new violation is diagnosed" || fail "new diagnosis missing: $out"

sed 's/ApprovalFloor/ChangedApprovalFloor/' \
    "$VIOLATION/cli/src/fno/company/contracts.py" > "$TMP/changed-contracts.py"
sed '$d' "$TMP/changed-contracts.py" > "$VIOLATION/cli/src/fno/company/contracts.py"
out="$(bash "$CHECK" --baseline --baseline-file "$FIXTURE_BASELINE" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "changed violation fails baseline" || fail "changed violation escaped: $out"
echo "$out" | grep -q 'new or changed violation' \
    && echo "$out" | grep -q 'resolved or changed baseline entry' \
    && ok "changed violation shows both halves" || fail "changed diagnosis incomplete: $out"

printf 'class WorkOrderRef:\n    pass\n' > "$VIOLATION/cli/src/fno/company/contracts.py"
out="$(bash "$CHECK" --baseline --baseline-file "$FIXTURE_BASELINE" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "reduction needs baseline update" || fail "stale baseline passed: $out"
echo "$out" | grep -q 'resolved or changed baseline entry' \
    && ok "stale baseline names removed debt" || fail "stale diagnosis missing: $out"

printf '%s\n' '# No known violations remain in this fixture.' > "$FIXTURE_BASELINE"
out="$(bash "$CHECK" --baseline --baseline-file "$FIXTURE_BASELINE" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "reduction plus baseline update passes" || fail "ratchet update failed: $out"
echo "$out" | grep -q '0 violations' \
    && ok "clean ratchet result is explicit" || fail "clean result missing: $out"

out="$(bash "$CHECK" --baseline --baseline-file "$TMP/missing.txt" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && ok "missing baseline fails closed" || fail "missing baseline exit $rc: $out"
printf '%s\n' 'opaque-count: 9' > "$FIXTURE_BASELINE"
out="$(bash "$CHECK" --baseline --baseline-file "$FIXTURE_BASELINE" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && ok "opaque baseline fails closed" || fail "opaque baseline exit $rc: $out"

echo "== package-granularity company map fails on the shipped topology shape =="
PACKAGE_MAP="$TMP/package-map"
make_repo "$PACKAGE_MAP"
{
    for _ in {1..15}; do echo '# padding'; done
    echo 'from fno.roles.models import RoleLayer'
} > "$PACKAGE_MAP/cli/src/fno/company/topology.py"
out="$(FNO_BOUNDARY_TEST_COMPANY_PACKAGE_CORE=1 bash "$CHECK" --strict "$PACKAGE_MAP" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "package map exits one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q \
    'cli/src/fno/company/topology.py:16: L1 core -> L2 roles / from fno.roles.models import RoleLayer' \
    && ok "package map names topology.py:16" || fail "topology evidence missing: $out"

echo "== ImportFrom aliases and package-relative imports cannot bypass the map =="
ALIASES="$TMP/aliases"
make_repo "$ALIASES"
printf 'from fno import roles\n' > "$ALIASES/cli/src/fno/company/contracts.py"
printf 'from .. import roles\n' > "$ALIASES/cli/src/fno/approvals/__init__.py"
out="$(bash "$CHECK" --strict "$ALIASES" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "alias forms exit one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q 'cli/src/fno/company/contracts.py:1: L1 core -> L2 roles / from fno import roles' \
    && ok "absolute package alias caught" || fail "absolute alias missed: $out"
[[ "$(echo "$out" | grep -c 'cli/src/fno/company/contracts.py:1: L1 core -> L2 roles')" -eq 1 ]] \
    && ok "absolute alias emits one finding" || fail "absolute alias duplicated: $out"
echo "$out" | grep -q 'cli/src/fno/approvals/__init__.py:1: L1 core -> L2 roles / from .. import roles' \
    && ok "relative package alias caught" || fail "relative alias missed: $out"

echo "== the public fno.company facade is part of the core layer =="
FACADE="$TMP/facade"
make_repo "$FACADE"
printf 'from fno.company import WorkOrderRef\n' > "$FACADE/cli/src/fno/paths.py"
out="$(bash "$CHECK" --strict "$FACADE" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "public facade edge exits one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q 'cli/src/fno/paths.py:1: L0 platform -> L1 core / from fno.company import WorkOrderRef' \
    && ok "public facade edge caught" || fail "public facade edge missed: $out"

echo "== pack-marked root files are attributed to their source pack =="
PROJECTION="$TMP/projection"
make_repo "$PROJECTION"
mkdir -p "$PROJECTION/plugins/growth-studio" "$PROJECTION/skills/growth-launch"
printf 'id: growth-studio\n' > "$PROJECTION/plugins/growth-studio/plugin.yaml"
printf '%s\n' '---' 'name: growth-marketer' 'pack: growth-studio' '---' \
    > "$PROJECTION/agents/growth-marketer.md"
printf '%s\n' '---' 'name: growth-launch' 'pack: growth-studio' '---' \
    > "$PROJECTION/skills/growth-launch/SKILL.md"
out="$(bash "$CHECK" --strict "$PROJECTION" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "valid projections stay clean" || fail "projection fixture failed: $out"
echo "$out" | grep -q 'agents/growth-marketer.md -> growth-studio' \
    && ok "agent projection attributed" || fail "agent attribution missing: $out"
echo "$out" | grep -q 'skills/growth-launch -> growth-studio' \
    && ok "skill projection attributed" || fail "skill attribution missing: $out"
echo "$out" | grep -q 'no enforcement for fno-skills' \
    && echo "$out" | grep -q 'fno-mux' \
    && ok "uncovered seams named honestly" || fail "coverage caveat missing: $out"

echo "== an orphaned pack marker is a violation =="
ORPHAN="$TMP/orphan"
make_repo "$ORPHAN"
printf '%s\n' '---' 'name: orphan' 'pack: no-such-pack' '---' \
    > "$ORPHAN/agents/orphan.md"
out="$(bash "$CHECK" --strict "$ORPHAN" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "orphan exits one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q \
    "agents/orphan.md:3: pack marker 'no-such-pack' names no plugins/no-such-pack/plugin.yaml" \
    && ok "orphan finding names file and marker" || fail "orphan evidence missing: $out"

echo "== checked-in baseline matches shipped findings while strict stays red =="
out="$(bash "$CHECK" --strict "$REPO_ROOT" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "repository strict audit stays red" || fail "strict audit exit $rc: $out"
# Counts come from the baseline file, not a literal: a burndown PR should not
# have to edit this test, and a hardcoded number silently rots into a rubber
# stamp once it stops matching reality. Sourcing both from the baseline also
# catches the case the literal never could -- strict and baseline disagreeing.
expected_violations="$(grep -c '^cli/.*L[0-9].* -> L[0-9]' "$REPO_BASELINE")"
actual_violations="$(echo "$out" | grep -c '^  cli/.*L[0-9].* -> L[0-9]')"
[[ "$actual_violations" -eq "$expected_violations" ]] \
    && ok "strict audit reports the $expected_violations baselined violations" \
    || fail "strict reports $actual_violations, baseline holds $expected_violations: $out"
expected_cycle="$(grep '^layer cycle: ' "$REPO_BASELINE" || true)"
if [[ -n "$expected_cycle" ]]; then
    echo "$out" | grep -q "  $expected_cycle" \
        && ok "strict audit retains the baselined cycle" \
        || fail "expected '$expected_cycle' in strict output: $out"
else
    echo "$out" | grep -q 'layer cycle: ' \
        && fail "baseline holds no cycle but strict reports one: $out" \
        || ok "no cycle in baseline, none in strict"
fi
out="$(bash "$CHECK" --baseline --baseline-file "$REPO_BASELINE" "$REPO_ROOT" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "repository baseline gate exits zero" || fail "repository baseline failed: $out"

echo ""
if [[ $FAILS -eq 0 ]]; then
    echo "test_check_company_boundaries: ALL PASS"
    exit 0
fi
echo "test_check_company_boundaries: $FAILS FAILED"
exit 1

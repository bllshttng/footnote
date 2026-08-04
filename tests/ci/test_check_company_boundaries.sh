#!/usr/bin/env bash
# Exercises the company boundary gate against hermetic fixture repositories.
set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
CHECK="$REPO_ROOT/scripts/ci/check-company-boundaries.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAILS=0
ok() { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS + 1)); }

make_repo() {
    local root="$1"
    mkdir -p "$root/cli/src/fno/company" "$root/cli/src/fno/roles" \
        "$root/agents" "$root/skills" "$root/plugins"
    printf 'class WorkOrderRef:\n    pass\n' > "$root/cli/src/fno/company/contracts.py"
    printf 'from fno.company.contracts import WorkOrderRef\n' > "$root/cli/src/fno/roles/models.py"
    printf 'from fno.roles.models import RoleLayer\n' > "$root/cli/src/fno/company/topology.py"
}

echo "== clean module-granularity map observes its positive control =="
CLEAN="$TMP/clean"
make_repo "$CLEAN"
out="$(bash "$CHECK" "$CLEAN" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "clean fixture exits zero" || fail "clean fixture failed: $out"
echo "$out" | grep -q "positive control ok" \
    && ok "positive control is reported" || fail "positive control missing: $out"
echo "$out" | grep -Eq '[0-9]+ layers, [0-9]+ modules, 0 violations' \
    && ok "clean result reports measured coverage" || fail "coverage missing: $out"

echo "== a broken scan fails closed =="
BROKEN="$TMP/broken"
make_repo "$BROKEN"
printf 'class RoleLayer:\n    pass\n' > "$BROKEN/cli/src/fno/roles/models.py"
out="$(bash "$CHECK" "$BROKEN" 2>&1)"; rc=$?
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
out="$(bash "$CHECK" "$VIOLATION" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "prohibited edge exits one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q \
    'cli/src/fno/company/contracts.py:12: L1 core -> L2 roles / from fno.roles.models import ApprovalFloor' \
    && ok "finding carries exact evidence" || fail "exact evidence missing: $out"
echo "$out" | grep -q 'layer cycle: L1 core -> L2 roles -> L1 core' \
    && ok "declared-layer cycle is printed" || fail "cycle missing: $out"

echo "== package-granularity company map fails on the shipped topology shape =="
PACKAGE_MAP="$TMP/package-map"
make_repo "$PACKAGE_MAP"
{
    for _ in {1..15}; do echo '# padding'; done
    echo 'from fno.roles.models import RoleLayer'
} > "$PACKAGE_MAP/cli/src/fno/company/topology.py"
out="$(FNO_BOUNDARY_TEST_COMPANY_PACKAGE_CORE=1 bash "$CHECK" "$PACKAGE_MAP" 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && ok "package map exits one" || fail "expected exit 1, got $rc: $out"
echo "$out" | grep -q \
    'cli/src/fno/company/topology.py:16: L1 core -> L2 roles / from fno.roles.models import RoleLayer' \
    && ok "package map names topology.py:16" || fail "topology evidence missing: $out"

echo ""
if [[ $FAILS -eq 0 ]]; then
    echo "test_check_company_boundaries: ALL PASS"
    exit 0
fi
echo "test_check_company_boundaries: $FAILS FAILED"
exit 1

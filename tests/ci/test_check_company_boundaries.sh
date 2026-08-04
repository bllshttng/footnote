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

echo ""
if [[ $FAILS -eq 0 ]]; then
    echo "test_check_company_boundaries: ALL PASS"
    exit 0
fi
echo "test_check_company_boundaries: $FAILS FAILED"
exit 1

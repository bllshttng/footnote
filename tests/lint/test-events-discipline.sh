#!/usr/bin/env bash
# tests/lint/test-events-discipline.sh
#
# Validates scripts/lint/events-discipline.sh against an ephemeral repo
# fixture. Each rule (bypass-echo, soft-outside-hooks, unwrapped-set-gate)
# gets a positive case (violation -> rc=1) and a tolerance case (legitimate
# pattern -> rc=0).

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
LINT="$REPO_ROOT/scripts/lint/events-discipline.sh"
fail=0

if [[ ! -r "$LINT" ]]; then
    echo "FAIL: $LINT missing"
    exit 1
fi

# Build an ephemeral repo (git is required by the lint).
make_fixture() {
    local d
    d=$(mktemp -d)
    (
        cd "$d"
        git init -q
        mkdir -p cli skills scripts hooks tests
        echo '#!/usr/bin/env bash' > cli/clean.sh
    )
    echo "$d"
}

cleanup() { [[ -n "${1:-}" && -d "$1" ]] && rm -rf "$1"; }

# AC1-HP: clean repo passes
d=$(make_fixture)
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 0 ]] || { echo "FAIL AC1-HP rc=$rc out=$out"; fail=1; }
cleanup "$d"

# AC2-ERR bypass-echo
d=$(make_fixture)
echo 'echo "{\"type\":\"foo\"}" >> .fno/events.jsonl' > "$d/skills/bad.sh"
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 1 ]] || { echo "FAIL AC2-bypass rc=$rc out=$out"; fail=1; }
[[ "$out" == *"events bypass at"* ]] || { echo "FAIL AC2-bypass diag: $out"; fail=1; }
cleanup "$d"

# AC4-EDGE: migrate-events-shape.py is allowed (legitimate rewrite)
d=$(make_fixture)
echo 'fout.write(json.dumps(new_row) + "\n")' > "$d/scripts/migrate-events-shape.py"
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 0 ]] || { echo "FAIL AC4-EDGE-migrate rc=$rc out=$out"; fail=1; }
cleanup "$d"

# AC2-ERR --soft outside hooks/
d=$(make_fixture)
echo 'bash emit-gate-transition.sh --soft' > "$d/cli/bad.sh"
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 1 ]] || { echo "FAIL AC2-soft rc=$rc out=$out"; fail=1; }
[[ "$out" == *"soft flag forbidden"* ]] || { echo "FAIL AC2-soft diag: $out"; fail=1; }
cleanup "$d"

# AC4-EDGE: --soft inside hooks/ is allowed
d=$(make_fixture)
echo 'bash emit-gate-transition.sh --soft' > "$d/hooks/legit.sh"
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 0 ]] || { echo "FAIL AC4-EDGE-soft-in-hook rc=$rc out=$out"; fail=1; }
cleanup "$d"

# AC2-ERR unwrapped set_gate
d=$(make_fixture)
cat > "$d/scripts/oops.sh" <<'INNER_EOF'
#!/usr/bin/env bash
bash scripts/lib/set-gate.sh state.md gate true phase
echo done
INNER_EOF
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 1 ]] || { echo "FAIL AC2-unwrapped rc=$rc out=$out"; fail=1; }
[[ "$out" == *"unwrapped set-gate"* ]] || { echo "FAIL AC2-unwrapped diag: $out"; fail=1; }
cleanup "$d"

# AC4-EDGE: set_gate wrapped in 'if !' is allowed
d=$(make_fixture)
cat > "$d/scripts/wrapped.sh" <<'INNER_EOF'
#!/usr/bin/env bash
if ! bash scripts/lib/set-gate.sh state.md gate true phase; then
    exit 1
fi
INNER_EOF
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 0 ]] || { echo "FAIL AC4-EDGE-if-wrapped rc=$rc out=$out"; fail=1; }
cleanup "$d"

# AC4-EDGE: set_gate under set -e at file scope is allowed
d=$(make_fixture)
cat > "$d/scripts/strictmode.sh" <<'INNER_EOF'
#!/usr/bin/env bash
set -euo pipefail
bash scripts/lib/set-gate.sh state.md gate true phase
INNER_EOF
out=$(cd "$d" && bash "$LINT" 2>&1)
rc=$?
[[ $rc -eq 0 ]] || { echo "FAIL AC4-EDGE-set-e rc=$rc out=$out"; fail=1; }
cleanup "$d"

if [[ $fail -ne 0 ]]; then
    echo ""
    echo "test-events-discipline: FAILED"
fi
exit $fail

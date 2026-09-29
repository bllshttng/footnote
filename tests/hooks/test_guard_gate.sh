#!/usr/bin/env bash
# tests/hooks/test_guard_gate.sh - the guardrail preset gate for the Python
# guards: a "disabled" answer answers the empty allow without running the
# guard; a clean enable runs it; anything that cannot answer (no binary, an
# old build, a crash) runs the guard unchanged.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

GATE="hooks/lib/guard-gate.sh"
[ -f "$GATE" ] || { echo "FAIL: $GATE missing"; exit 1; }
bash -n "$GATE" || { echo "FAIL: $GATE has a syntax error"; exit 1; }

BASE="$(mktemp -d -t guard-gate-XXXXXX)"
trap 'rm -rf "$BASE"' EXIT
mkdir -p "$BASE/bin"

# A stub fno-agents whose guard-enabled answers what the case needs.
cat > "$BASE/bin/fno-agents" <<'STUB'
#!/usr/bin/env bash
exit "${STUB_RC:-0}"
STUB
chmod +x "$BASE/bin/fno-agents"
RPATH="$BASE/bin:/usr/bin:/bin"

# 1. disabled (exit 1): empty allow at exit 0, the guard command never runs.
out="$(printf 'aaa' | STUB_RC=1 PATH="$RPATH" bash "$GATE" recursive-grep tr a b 2>&1)"
[ "$out" = "{}" ] || { echo "FAIL disabled arm: got: $out"; exit 1; }

# 2. enabled (exit 0): the guard command runs on the piped stdin.
out="$(printf 'aaa' | STUB_RC=0 PATH="$RPATH" bash "$GATE" recursive-grep tr a b 2>&1)"
[ "$out" = "bbb" ] || { echo "FAIL enabled arm: got: $out"; exit 1; }

# 3. no fno-agents: the guard runs unchanged (fail closed).
out="$(printf 'aaa' | PATH="/usr/bin:/bin" bash "$GATE" recursive-grep tr a b 2>&1)"
[ "$out" = "bbb" ] || { echo "FAIL no-binary arm: got: $out"; exit 1; }

echo "PASS: guard-gate answers the preset's no, runs on yes, and fails closed"

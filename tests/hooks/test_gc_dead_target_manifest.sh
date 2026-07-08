#!/usr/bin/env bash
# test_gc_dead_target_manifest.sh - x-4af4 T3: the session-start GC archives a
# DEAD target manifest (owning session gone) and leaves a LIVE one in place.
# Hermetic: stubs `fno` so `target status`/`state archive` need no real graph.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HELPER="$REPO_ROOT/hooks/helpers/gc-dead-target-manifest.sh"

pass=0
fail=0
check_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $desc"; pass=$((pass + 1))
    else
        echo "FAIL: $desc (expected='$expected' actual='$actual')"; fail=$((fail + 1))
    fi
}
check_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if printf '%s' "$haystack" | grep -qF "$needle"; then
        echo "PASS: $desc"; pass=$((pass + 1))
    else
        echo "FAIL: $desc (needle='$needle' not in: $haystack)"; fail=$((fail + 1))
    fi
}

# Sandbox with a stubbed `fno` whose `target status --json` reports $1 as the
# manifest-live verdict, and whose `state archive` moves the file like the real
# verb. Returns the sandbox dir.
make_sandbox() {
    local verdict="$1" dir
    dir="$(mktemp -d)"
    mkdir -p "$dir/bin" "$dir/.fno"
    cat > "$dir/bin/fno" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == "target" && "\$2" == "status" ]]; then
  echo '{ "node": "x-1", "manifest-live": "${verdict} (test)" }'
elif [[ "\$1" == "state" && "\$2" == "archive" ]]; then
  p=""; shift 2
  while [[ \$# -gt 0 ]]; do [[ "\$1" == "--path" ]] && p="\$2"; shift; done
  [[ -n "\$p" ]] && mv "\$p" "\$p.archived.test.md"
fi
STUB
    chmod +x "$dir/bin/fno"
    printf '%s' "$dir"
}

# 1. DEAD manifest -> archived + note printed.
dir="$(make_sandbox dead)"
echo "attended: false" > "$dir/.fno/target-state.md"
out="$(cd "$dir" && PATH="$dir/bin:$PATH" bash "$HELPER" .fno/target-state.md 2>&1)"
archived=0
[[ -f "$dir/.fno/target-state.md.archived.test.md" && ! -f "$dir/.fno/target-state.md" ]] && archived=1
check_eq "dead manifest is archived (moved aside)" "1" "$archived"
check_contains "prints a one-line archive note" "archived dead target manifest" "$out"
rm -rf "$dir"

# 2. LIVE manifest -> left in place (AC2-EDGE: never archive a running session).
dir="$(make_sandbox live)"
echo "attended: false" > "$dir/.fno/target-state.md"
(cd "$dir" && PATH="$dir/bin:$PATH" bash "$HELPER" .fno/target-state.md) >/dev/null 2>&1
kept=0
[[ -f "$dir/.fno/target-state.md" && ! -f "$dir/.fno/target-state.md.archived.test.md" ]] && kept=1
check_eq "live manifest is left in place" "1" "$kept"
rm -rf "$dir"

# 3. No manifest -> clean no-op (advisory, exit 0).
dir="$(make_sandbox dead)"
rc=0
(cd "$dir" && PATH="$dir/bin:$PATH" bash "$HELPER" .fno/target-state.md) || rc=$?
check_eq "no manifest is a clean no-op" "0" "$rc"
rm -rf "$dir"

echo ""
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]

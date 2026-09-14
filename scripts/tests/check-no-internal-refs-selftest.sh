#!/usr/bin/env bash
# Positive + negative control for check-no-internal-refs.sh.
#
# AGENTS.md principle 6 cites this gate for the no-node-ids-in-comments rule,
# but until the code-scope widening the gate never scanned a single line of
# code: its scope was the prose surfaces only. A gate a rule cites must be
# watched failing, so this asserts, per leak class and per scope, that a
# planted violation fails and that each sanctioned form still passes.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GATE="$REPO_ROOT/scripts/ci/check-no-internal-refs.sh"
TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_ROOT"' EXIT

FIXTURE_ROOT="$TMP_ROOT/repo"
git clone --quiet --no-hardlinks "$REPO_ROOT" "$FIXTURE_ROOT"
# git clone carries the COMMITTED tree; this run's cleanup lives in the working
# tree, and renames mean some committed files must NOT exist in the fixture at
# all. Mirror the working tree over the clone, deleting anything the working
# tree no longer has (.git excepted).
rsync -a --delete --exclude='.git/' "$REPO_ROOT/" "$FIXTURE_ROOT/"
run_gate() { ( cd "$FIXTURE_ROOT" && bash scripts/ci/check-no-internal-refs.sh 2>&1 ); }

if run_gate >/dev/null; then
    :  # clean tree passes (asserted on the fixture clone, which is clean)
else
    echo "FAIL: clean fixture must pass the internal-refs gate" >&2
    run_gate >&2 || true
    exit 1
fi

# Plant $body at $path in the fixture and expect the gate to FAIL naming it.
# The probe is staged (git add) because the gate enumerates tracked files via
# git ls-files: an untracked probe would be invisible to it.
assert_rejected() {
    local label="$1" path="$2" body="$3"
    local probe="$FIXTURE_ROOT/$path"
    local output="" actual_exit=0

    mkdir -p "$(dirname "$probe")"
    printf '%s\n' "$body" > "$probe"
    git -C "$FIXTURE_ROOT" add -f "$path"
    output="$(run_gate)" || actual_exit=$?
    git -C "$FIXTURE_ROOT" rm -q --cached "$path" >/dev/null
    rm -f "$probe"

    if [[ "$actual_exit" -eq 0 ]]; then
        echo "FAIL: $label passed the gate" >&2
        exit 1
    fi
    if ! grep -qF "$path" <<<"$output"; then
        echo "FAIL: $label was rejected but the report did not name $path" >&2
        exit 1
    fi
    echo "  ok: $label rejected"
}

# Plant $body at $path in the fixture and expect the gate to PASS.
assert_accepted() {
    local label="$1" path="$2" body="$3"
    local probe="$FIXTURE_ROOT/$path"
    local actual_exit=0

    mkdir -p "$(dirname "$probe")"
    printf '%s\n' "$body" > "$probe"
    git -C "$FIXTURE_ROOT" add -f "$path"
    run_gate >/dev/null 2>&1 || actual_exit=$?
    git -C "$FIXTURE_ROOT" rm -q --cached "$path" >/dev/null
    rm -f "$probe"

    if [[ "$actual_exit" -ne 0 ]]; then
        echo "FAIL: $label must pass the gate" >&2
        run_gate >&2 || true
        exit 1
    fi
    echo "  ok: $label accepted"
}

# --- the widened code scope must catch what the prose scope never saw ---
assert_rejected "node id in a Rust doc comment" \
    "crates/fno/src/_selftest_probe.rs" '// planned by node x-1b88 for the arms readout.'
assert_rejected "node id in a Python help string" \
    "cli/src/fno/_selftest_probe.py" 'help="planned by node x-1b88."'
assert_rejected "node id in a shell comment" \
    "scripts/_selftest_probe.sh" '# planned by node x-1b88.'
assert_rejected "ab- id in a code comment" \
    "hooks/_selftest_probe.sh" '# legacy plan ab-1b88c2d4 shaped this.'
assert_rejected "session URL in code" \
    "crates/fno-agents/src/_selftest_probe.rs" '// transcript: https://claude.ai/code/1a2b3c'
assert_rejected "node id still fails in the prose scope" \
    "docs/_selftest_probe.md" "a breadcrumb (x-1b88)."

# --- sanctioned forms must still pass ---
assert_accepted "allowlisted synthetic in code comment" \
    "crates/fno/src/_selftest_probe.rs" '// format example: tgt-x-aaaa-liveness.'
assert_accepted "internal/ path literal in code" \
    "cli/src/fno/_selftest_probe.py" 'RESOLVER = "internal/fno/plans"'
assert_accepted "node id in a test path is exempt" \
    "scripts/tests/_selftest_probe.py" 'NODE = "x-1b88"  # fixture data'
assert_accepted "node id in an inline test module name" \
    "crates/fno/src/_selftest_probe_tests.rs" 'const NODE: &str = "x-1b88";'

echo "PASS: check-no-internal-refs catches planted leaks and passes sanctioned forms"

#!/usr/bin/env bash
# scripts/ci/check-registry-schema-parity.sh
#
# Cross-language registry schema_version parity check (ab-0baecaed).
#
# The fno-agents registry.json is read and written by two implementations
# that MUST agree on the current write-version:
#
#   - Rust  : REGISTRY_SCHEMA_VERSION in crates/fno-agents/src/state.rs
#   - Python: SCHEMA_VERSION         in cli/src/fno/agents/registry.py
#
# A bump on one side without the other lets the two implementations write
# divergent schema_version values into the same store. The readers' accepted
# -range guards then mis-handle it: a reader pinned below the new version
# rejects the store ("upgrade fno"), while a stale reader can silently
# mis-reconcile a forward-compatible field it does not understand. PR #375
# bumped both to v4 in lockstep by hand; this check makes that lockstep
# mechanical so a future bump cannot drift the two sides apart.
#
# This is a pure text-extraction check: it reads the two source files and
# compares the declared constants. No build and no Rust binary are required,
# so it is cheap enough to run in both cli-ci.yml (fires on cli/** changes,
# catching a Python-only bump) and rust-ci.yml (fires on crates/** changes,
# catching a Rust-only bump).
#
# Exit codes:
#   0  versions match (and both parsed as positive integers)
#   1  mismatch, or a version could not be extracted / is non-numeric /
#      is not a positive integer (a 0 bump would stamp registries both
#      readers reject as out-of-range, so it is refused here)
#   2  usage error
#
# Flags:
#   --rust-file PATH    override the Rust source   (default: canonical path)
#   --python-file PATH  override the Python source (default: canonical path)
#   --selftest          run built-in fixtures proving the check detects
#                       match / mismatch / unextractable, then exit

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

RUST_FILE="${REPO_ROOT}/crates/fno-agents/src/state.rs"
PYTHON_FILE="${REPO_ROOT}/cli/src/fno/agents/registry.py"
SELFTEST=0

# ── Extraction ─────────────────────────────────────────────────────────
# Each extractor isolates the constant's declaration line and pulls the
# integer on the right-hand side of `=`. Emits nothing (empty) when the
# constant is absent, which the caller treats as a hard error rather than
# letting "" == "" pass silently.

extract_rust() {
    # Matches: pub const REGISTRY_SCHEMA_VERSION: u32 = 4;
    local f="$1" line
    line=$(grep -E 'const[[:space:]]+REGISTRY_SCHEMA_VERSION[[:space:]]*:' "$f" 2>/dev/null | head -n 1)
    [[ -n "$line" ]] || return 0
    sed -E 's/.*=[[:space:]]*([0-9]+).*/\1/' <<<"$line"
}

extract_python() {
    # Matches: SCHEMA_VERSION = 4   (anchored to ^ so it does not capture
    # JSON_SCHEMA_VERSION in format.py or any *_SCHEMA_VERSION sibling).
    local f="$1" line
    line=$(grep -E '^SCHEMA_VERSION[[:space:]]*=' "$f" 2>/dev/null | head -n 1)
    [[ -n "$line" ]] || return 0
    sed -E 's/.*=[[:space:]]*([0-9]+).*/\1/' <<<"$line"
}

# ── Comparison ─────────────────────────────────────────────────────────
# Returns 0 on parity, 1 on mismatch / unextractable. Prints a verdict to
# stdout (OK) or stderr (failure). Pure function: no global state read.

check_parity() {
    local rust_file="$1" python_file="$2"
    local rust_ver python_ver

    [[ -f "$rust_file" ]]   || { echo "ERROR: Rust source not found: $rust_file" >&2; return 1; }
    [[ -f "$python_file" ]] || { echo "ERROR: Python source not found: $python_file" >&2; return 1; }

    rust_ver=$(extract_rust "$rust_file")
    python_ver=$(extract_python "$python_file")

    # Positive integers only. A bare ^[0-9]+$ would accept 0, but both
    # readers gate the store on 1..=SCHEMA_VERSION, so a 0 bump would stamp
    # registries that every reader rejects while this guard reported parity.
    if [[ ! "$rust_ver" =~ ^[1-9][0-9]*$ ]]; then
        echo "ERROR: REGISTRY_SCHEMA_VERSION in $rust_file is missing or not a positive integer (got '${rust_ver}')" >&2
        return 1
    fi
    if [[ ! "$python_ver" =~ ^[1-9][0-9]*$ ]]; then
        echo "ERROR: SCHEMA_VERSION in $python_file is missing or not a positive integer (got '${python_ver}')" >&2
        return 1
    fi

    if [[ "$rust_ver" != "$python_ver" ]]; then
        echo "ERROR: registry schema_version mismatch." >&2
        echo "  Rust   REGISTRY_SCHEMA_VERSION = $rust_ver  ($rust_file)" >&2
        echo "  Python SCHEMA_VERSION          = $python_ver  ($python_file)" >&2
        echo "  Bump both sides in lockstep (see PR #375 for the v4 precedent)." >&2
        return 1
    fi

    echo "registry schema parity OK: Rust == Python == $rust_ver"
    return 0
}

# ── Selftest ───────────────────────────────────────────────────────────
# Proves the detector actually detects: a check that can only ever pass is
# worse than no check. Drives the real script (via $0) over synthetic
# fixtures and asserts the expected exit codes.

run_selftest() {
    local tmp rc fails=0
    # Explicit template path (no `-t`): BSD mktemp treats `-t arg` as a
    # prefix and leaves the literal XXXXXX in the name, GNU substitutes it;
    # spelling the full path makes both behave the same.
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/registry-parity-selftest.XXXXXX")
    trap 'rm -rf "$tmp"' RETURN

    local expect_pass="expected exit 0" expect_fail="expected exit 1"

    # Case 1: matching versions -> pass (0)
    printf 'pub const REGISTRY_SCHEMA_VERSION: u32 = 4;\n' > "$tmp/match.rs"
    printf 'SCHEMA_VERSION = 4\n' > "$tmp/match.py"
    "${BASH_SOURCE[0]}" --rust-file "$tmp/match.rs" --python-file "$tmp/match.py" >/dev/null 2>&1
    rc=$?
    if [[ "$rc" == "0" ]]; then echo "  PASS: matching versions accepted"
    else echo "  FAIL: matching versions ($expect_pass, got $rc)"; fails=$((fails + 1)); fi

    # Case 2: mismatch -> fail (1)
    printf 'pub const REGISTRY_SCHEMA_VERSION: u32 = 4;\n' > "$tmp/mismatch.rs"
    printf 'SCHEMA_VERSION = 5\n' > "$tmp/mismatch.py"
    "${BASH_SOURCE[0]}" --rust-file "$tmp/mismatch.rs" --python-file "$tmp/mismatch.py" >/dev/null 2>&1
    rc=$?
    if [[ "$rc" == "1" ]]; then echo "  PASS: mismatch rejected"
    else echo "  FAIL: mismatch ($expect_fail, got $rc)"; fails=$((fails + 1)); fi

    # Case 3: Rust constant absent -> fail (1), not a silent empty==empty pass
    printf '// no schema constant here\n' > "$tmp/absent.rs"
    printf 'SCHEMA_VERSION = 4\n' > "$tmp/absent.py"
    "${BASH_SOURCE[0]}" --rust-file "$tmp/absent.rs" --python-file "$tmp/absent.py" >/dev/null 2>&1
    rc=$?
    if [[ "$rc" == "1" ]]; then echo "  PASS: unextractable Rust version rejected"
    else echo "  FAIL: unextractable Rust ($expect_fail, got $rc)"; fails=$((fails + 1)); fi

    # Case 4: u32 type token must not be mistaken for the value
    printf 'pub const REGISTRY_SCHEMA_VERSION: u32 = 7;\n' > "$tmp/u32.rs"
    printf 'SCHEMA_VERSION = 7\n' > "$tmp/u32.py"
    "${BASH_SOURCE[0]}" --rust-file "$tmp/u32.rs" --python-file "$tmp/u32.py" >/dev/null 2>&1
    rc=$?
    if [[ "$rc" == "0" ]]; then echo "  PASS: u32 token not mistaken for the value"
    else echo "  FAIL: u32 token parse ($expect_pass, got $rc)"; fails=$((fails + 1)); fi

    # Case 5: both sides = 0 -> fail (1). Zero parses as an integer but is
    # outside the readers' 1..=N accepted range, so parity at 0 is a trap.
    printf 'pub const REGISTRY_SCHEMA_VERSION: u32 = 0;\n' > "$tmp/zero.rs"
    printf 'SCHEMA_VERSION = 0\n' > "$tmp/zero.py"
    "${BASH_SOURCE[0]}" --rust-file "$tmp/zero.rs" --python-file "$tmp/zero.py" >/dev/null 2>&1
    rc=$?
    if [[ "$rc" == "1" ]]; then echo "  PASS: zero version rejected"
    else echo "  FAIL: zero version ($expect_fail, got $rc)"; fails=$((fails + 1)); fi

    if [[ "$fails" -gt 0 ]]; then
        echo "registry-schema-parity selftest: $fails failure(s)" >&2
        return 1
    fi
    echo "registry-schema-parity selftest: all cases passed"
    return 0
}

# ── Arg parsing ────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --rust-file)   RUST_FILE="$2"; shift 2 ;;
        --python-file) PYTHON_FILE="$2"; shift 2 ;;
        --selftest)    SELFTEST=1; shift ;;
        -h|--help)     grep -E '^#( |$)' "$0" | sed -E 's/^# ?//'; exit 0 ;;
        *)             echo "ERROR: unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ "$SELFTEST" == "1" ]]; then
    run_selftest
    exit $?
fi

check_parity "$RUST_FILE" "$PYTHON_FILE"
exit $?

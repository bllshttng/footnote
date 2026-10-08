#!/usr/bin/env bash
# check-state-root-writes.sh - a NEW root-level write without an inventory
# row fails.
#
# The runtime half of the state-root tidiness rule is the paths module: a
# root-level leaf resolves through paths.root_state_file(), which refuses any
# name the inventory doc does not row (docs/state-root-inventory.md, "The
# rule"), and state_runtime_file()/logs_file() are the sanctioned subfolder
# doors. This gate is the review half: it fails a PR whose diff ADDS a
# hand-built root write that skips those doors, in either language:
#
#   - Python `state_dir() / "<leaf>"` with a bare leaf: refused unless the
#     leaf is rowed (the shrink-only baseline, or the fence inside paths.py).
#   - Python `root_state_file("<leaf>")` naming a leaf outside the fence:
#     refused here rather than at the first runtime call.
#   - Rust `place(<root>, "<name>")` for a name the layout table does not
#     carry: place() answers root/<name> unchanged for an unknown row, which
#     is exactly the silent root write this gate exists to catch.
#
# `state_runtime_file("<any>")` and `logs_file("<any>")` never refuse: a
# named subfolder is the remedy the rule grants.
#
# Run: bash scripts/ci/check-state-root-writes.sh [--self-test]
# Env: PR_BASE_REF (default main) names the base the diff reads against.
# Exit: 0 pass, 1 an added root write, 2 misuse or unreadable inputs.

set -euo pipefail

SELF_TEST=0
for arg in "$@"; do
    case "$arg" in
        --self-test) SELF_TEST=1 ;;
        -h | --help) sed -n '2,/^set -/{/^set -/q;s/^# \{0,1\}//p;}' "$0"; exit 0 ;;
        *) echo "check-state-root-writes: unknown arg: $arg" >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE="$REPO_ROOT/scripts/ci/state-root-rows.baseline"
LAYOUT="$REPO_ROOT/docs/state-root-layout.tsv"

# ADDED_LINES BASELINE_FILE LAYOUT_FILE -> verdict. Exit 0 pass, 1 refused.
check_lines() {
    (cd "$REPO_ROOT/cli" && uv run --frozen python - "$1" "$2" "$3" <<'PY'
import fnmatch
import re
import sys
from pathlib import Path

sys.path.insert(0, "src")
from fno.paths import _ROOT_STATE_FILE_ROWS

added_text, baseline_path, layout_path = (
    Path(sys.argv[1]).read_text(encoding="utf-8"),
    Path(sys.argv[2]),
    Path(sys.argv[3]),
)

# A leaf is rowed when a baseline pattern matches it exactly or by glob.
patterns = [
    entry
    for line in baseline_path.read_text(encoding="utf-8").splitlines()
    if (entry := line.partition("#")[0].strip())
]


def rowed(leaf: str) -> bool:
    return any(fnmatch.fnmatchcase(leaf, pat) for pat in patterns)


layout_rows = {
    line.split("\t")[0]
    for line in layout_path.read_text(encoding="utf-8").splitlines()
    if line.strip() and not line.lstrip().startswith("#")
}

PY_STATE_LEAF = re.compile(r'state_dir\(\)\s*/\s*"([A-Za-z0-9_.\-]+)"')
PY_DOOR = re.compile(r'root_state_file\(\s*"([^"]+)"')
# One nesting level in the receiver: place(&state_root(cwd), "name").
RUST_PLACE = re.compile(r'place\(\s*(?:[^,()]|\([^()]*\))+,\s*"([^"]+)"\s*\)')

refused = []
for line in added_text.splitlines():
    body = line[1:] if line.startswith("+") else line
    for match in PY_STATE_LEAF.finditer(body):
        leaf = match.group(1)
        if not rowed(leaf) and leaf not in _ROOT_STATE_FILE_ROWS:
            refused.append(
                f"{line}\n  state_dir() / \"{leaf}\": '{leaf}' has no state-root "
                "inventory row. The root holds no new writers "
                "(docs/state-root-inventory.md, 'The rule'); use "
                "paths.state_runtime_file() / logs_file(), or an existing rowed "
                "accessor."
            )
    for match in PY_DOOR.finditer(body):
        leaf = match.group(1)
        if leaf not in _ROOT_STATE_FILE_ROWS:
            refused.append(
                f"{line}\n  root_state_file(\"{leaf}\"): not in the fence; the "
                "runtime door would raise on the first call."
            )
    for match in RUST_PLACE.finditer(body):
        name = match.group(1)
        if name not in layout_rows:
            refused.append(
                f"{line}\n  place(..., \"{name}\"): the layout table "
                f"(docs/state-root-layout.tsv) has no '{name}' row, so place() "
                f"answers the bare root spelling - a silent root write. Add a "
                "row or resolve through a subfolder accessor."
            )

if refused:
    for entry in refused:
        print(entry, file=sys.stderr)
    print(
        "check-state-root-writes: the diff adds a root-level write without an "
        "inventory row.",
        file=sys.stderr,
    )
    sys.exit(1)
print("check-state-root-writes: ok - every added write is rowed or subfoldered")
PY
    )
}

if [[ "$SELF_TEST" == 1 ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    printf 'ledger.json\nevents.jsonl\nconfig.toml.bak*\nstate\n' >"$tmp/baseline"
    printf 'ledger.json\tdb/ledger.json\toverwrite\tdaemon\n' >"$tmp/layout"
    # Positive control: an unrowed bare-leaf write must refuse.
    printf '+    p = state_dir() / "new-junk.out"\n' >"$tmp/added-bad"
    if check_lines "$tmp/added-bad" "$tmp/baseline" "$tmp/layout" 2>/dev/null; then
        echo "check-state-root-writes: self-test: an unrowed root write must refuse" >&2
        exit 2
    fi
    # A rowed leaf passes.
    printf '+    p = state_dir() / "ledger.json"\n' >"$tmp/added-good"
    check_lines "$tmp/added-good" "$tmp/baseline" "$tmp/layout" >/dev/null || {
        echo "check-state-root-writes: self-test: a rowed leaf must pass" >&2
        exit 2
    }
    # A glob row admits its match.
    printf '+    p = state_dir() / "config.toml.bak-20260101"\n' >"$tmp/added-glob"
    check_lines "$tmp/added-glob" "$tmp/baseline" "$tmp/layout" >/dev/null || {
        echo "check-state-root-writes: self-test: a glob-matched leaf must pass" >&2
        exit 2
    }
    # A Rust place() for an untabled name must refuse.
    printf '+    let p = crate::state_layout::place(&root, "untabled.db");\n' >"$tmp/added-rust"
    if check_lines "$tmp/added-rust" "$tmp/baseline" "$tmp/layout" 2>/dev/null; then
        echo "check-state-root-writes: self-test: an untabled place() must refuse" >&2
        exit 2
    fi
    echo "check-state-root-writes: self-test passed"
    exit 0
fi

if [[ ! -f "$BASELINE" || ! -f "$LAYOUT" ]]; then
    echo "check-state-root-writes: $BASELINE or $LAYOUT is missing" >&2
    exit 2
fi

BASE_REF="${PR_BASE_REF:-main}"
if ! git rev-parse --verify --quiet "origin/${BASE_REF}" >/dev/null; then
    echo "check-state-root-writes: origin/${BASE_REF} unreadable; pass PR_BASE_REF" >&2
    exit 2
fi

ADDED="$(mktemp)"
trap 'rm -f "$ADDED"' EXIT
git diff --diff-filter=A -U0 "origin/${BASE_REF}...HEAD" \
    -- cli/src/fno hooks scripts crates >"$ADDED"

check_lines "$ADDED" "$BASELINE" "$LAYOUT"

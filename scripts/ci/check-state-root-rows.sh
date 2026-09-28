#!/usr/bin/env bash
# check-state-root-rows.sh - the inventory doc's root rows are SHRINK-ONLY.
#
# The state-root tidiness rule (docs/state-root-inventory.md, "The rule"):
# the root holds folders plus the rows it already has, and nothing new lands
# at the top level. A writer that moves into a subfolder deletes its row; a
# PR that ADDS a root pattern to the doc refuses. The baseline is the
# checked-in pattern list at the freeze (scripts/ci/state-root-rows.baseline),
# so the backfill PR itself passes and every later PR diffs against frozen
# names: added = refused, removed = banked. Regenerate the baseline in the
# same PR as a removal to bank it for good.
#
# The extractor is fno.graph._state_root_inventory.top_level_patterns - the
# same parser the drift report and the Rust reading use, so one dialect
# decides everywhere. It runs through uv from cli/ because importing the
# module pulls the fno.graph package (and its yaml dep) with it.
#
# Run: bash scripts/ci/check-state-root-rows.sh [--self-test] [--quiet]
# Exit: 0 pass, 1 an added row, 2 misuse, a missing baseline, or a doc that
#       cannot be read - a gate that cannot see its inputs never passes.

set -euo pipefail

SELF_TEST=0
QUIET=0
for arg in "$@"; do
    case "$arg" in
        --self-test) SELF_TEST=1 ;;
        --quiet) QUIET=1 ;;
        -h | --help) sed -n '2,/^set -/{/^set -/q;s/^# \{0,1\}//p;}' "$0"; exit 0 ;;
        *) echo "check-state-root-rows: unknown arg: $arg" >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
DOC="docs/state-root-inventory.md"
BASELINE="scripts/ci/state-root-rows.baseline"

# DOC BASELINE QUIET -> subset verdict. Exit 0 subset, 1 added, 2 unreadable.
check_rows() {
    (cd "$REPO_ROOT/cli" && uv run --frozen python - "$1" "$2" "$3" <<'PY'
import sys
from pathlib import Path

sys.path.insert(0, "src")
from fno.graph._state_root_inventory import _BACKTICK, _PLACEHOLDER, top_level_patterns

doc, baseline, quiet = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3] == "--quiet"
if not doc.is_file() or not baseline.is_file():
    print(f"check-state-root-rows: {doc} or {baseline} is unreadable", file=sys.stderr)
    sys.exit(2)
head = set(top_level_patterns(doc))
base = {line.strip() for line in baseline.read_text().splitlines() if line.strip()}
banked = sorted(base - head)
added = sorted(head - base)

# Folder-row provenance: a row whose Entry span holds "/" documents a folder,
# and the parser emits its first segment. The root holds folders, so an added
# pattern that comes only from such spans is lawful; the same span parsing
# (module regexes, not a second dialect) also collects the bare-name yields,
# which stay refused - a bare row may never enter, folder or not.
import re

folder_names, bare_names = set(), set()
for line in doc.read_text(encoding="utf-8").splitlines():
    if not line.lstrip().startswith("|"):
        continue
    cells = line.split("|")
    if len(cells) < 2:
        continue
    spans = [s for s in (_PLACEHOLDER.sub("*", t.strip()) for t in _BACKTICK.findall(cells[1])) if s]
    slashed = any("/" in s for s in spans)
    bases = [s for s in spans if "/" not in s and "*" not in s and not s.startswith(".")]
    for span in spans:
        if "/" in span:
            folder_names.add(span.split("/", 1)[0])
        elif not slashed:
            bare_names.add(span)
            if span.startswith(".") and len(span) > 1 and "*" not in span:
                for b in bases:
                    bare_names.add(b + span)

lawful = sorted(p for p in added if p in folder_names and p not in bare_names)
refused = sorted(p for p in added if p not in folder_names or p in bare_names)
if not quiet:
    for pattern in banked:
        print(f"banked: {pattern}")
    for pattern in lawful:
        print(f"folder: {pattern}")
if refused:
    for pattern in refused:
        print(f"added: {pattern}", file=sys.stderr)
        if pattern in folder_names:
            print(
                f"check-state-root-rows: '{pattern}' names a folder; admit it with "
                f"rows like '{pattern}/<file>', never a bare folder row.",
                file=sys.stderr,
            )
    print(
        "check-state-root-rows: the inventory doc grew a root row; root rows are shrink-only. "
        "The remedy is a subfolder: put the state under a named directory and do not add the row.",
        file=sys.stderr,
    )
    sys.exit(1)
if not quiet:
    print("check-state-root-rows: ok - the root rows only shrank")
PY
    )
}

if [[ "$SELF_TEST" == 1 ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    printf '| `graph.db` | store |\n| `backups/` | rotation |\n' >"$tmp/base.md"
    printf 'backups\ngraph.db\n' >"$tmp/baseline.txt"
    printf '| `graph.db` | store |\n| `backups/` | rotation |\n| `new-junk.out` | someone |\n' >"$tmp/grown.md"
    printf '| `graph.db` | store |\n' >"$tmp/shrunk.md"
    check_rows "$tmp/base.md" "$tmp/baseline.txt" --quiet || {
        echo "check-state-root-rows: self-test: a doc matching its baseline must pass" >&2
        exit 2
    }
    if check_rows "$tmp/grown.md" "$tmp/baseline.txt" --quiet 2>/dev/null; then
        echo "check-state-root-rows: self-test: an added row must refuse (positive control missed)" >&2
        exit 2
    fi
    printf '| `graph.db` | store |\n| `backups/` | rotation |\n| `newdir/x.json` | someone |\n' >"$tmp/grown-folder.md"
    check_rows "$tmp/grown-folder.md" "$tmp/baseline.txt" --quiet || {
        echo "check-state-root-rows: self-test: an added folder row must pass" >&2
        exit 2
    }
    printf '| `graph.db` | store |\n| `backups/` | rotation |\n| `newdir/x.json` | someone |\n| `newdir` | bare folder |\n' >"$tmp/grown-bare-folder.md"
    if check_rows "$tmp/grown-bare-folder.md" "$tmp/baseline.txt" --quiet 2>/dev/null; then
        echo "check-state-root-rows: self-test: a bare folder row must refuse" >&2
        exit 2
    fi
    check_rows "$tmp/shrunk.md" "$tmp/baseline.txt" --quiet || {
        echo "check-state-root-rows: self-test: a shrunk doc must pass" >&2
        exit 2
    }
    echo "check-state-root-rows: self-test passed"
    exit 0
fi

if [[ ! -f "$REPO_ROOT/$BASELINE" || ! -f "$REPO_ROOT/$DOC" ]]; then
    echo "check-state-root-rows: $DOC or $BASELINE is missing under $REPO_ROOT" >&2
    exit 2
fi

check_rows "$REPO_ROOT/$DOC" "$REPO_ROOT/$BASELINE" "$( [[ "$QUIET" == 1 ]] && echo --quiet || echo )"

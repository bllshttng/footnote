#!/usr/bin/env bash
# check-coverage-context-parity.sh - one context string, one override label,
# every surface that spells them.
#
# The `fno/review-coverage` commit-status context and the `coverage-override`
# label each live in several places: the Python publisher (the canonical
# consts), the ruleset data the operator applies, the Rust publisher, the
# refresher workflow, and the post-merge audit. Each suite pins only its own
# copy, so a rename in any one of them keeps every CI check green while the
# required context sits pending forever, or the audit reads a context nobody
# writes. This check reads the strings off the surfaces and fails on any
# drift, naming the surface that moved.
#
# Every needle is the surface's EXACT syntactic form, not a substring: a
# renamed `fno/review-coverage-v2` still contains `fno/review-coverage`, so a
# bare grep would pass exactly the drift this check exists to catch.
#
# Exit: 0 every surface agrees, 1 drift (the message names the surface)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

fail=0
mismatch() { echo "FAIL: $1" >&2; fail=1; }

expect_fixed() { # <file> <fixed needle> <surface>
  if ! grep -qF -- "$2" "$1"; then
    mismatch "$3 does not carry '$2'"
  fi
}
expect_line() { # <file> <ERE anchored to a full line> <surface>
  if ! grep -qE -- "$2" "$1"; then
    mismatch "$3 does not carry '$2'"
  fi
}

# The canonical strings are the Python consts: the publisher is the writer
# every other surface certifies. An unreadable const is a hard failure, never
# an empty comparison that would pass vacuously.
py_ctx="$(grep -oE 'COVERAGE_STATUS_CONTEXT = "[^"]+"' \
  "$ROOT/cli/src/fno/pr/_reviews.py" | head -1 | sed 's/.*"\(.*\)"/\1/')"
if [ -z "$py_ctx" ]; then
  echo "FAIL: cannot read COVERAGE_STATUS_CONTEXT from cli/src/fno/pr/_reviews.py" >&2
  exit 1
fi
py_label="$(grep -oE 'COVERAGE_OVERRIDE_LABEL = "[^"]+"' \
  "$ROOT/cli/src/fno/pr/_reviews.py" | head -1 | sed 's/.*"\(.*\)"/\1/')"
if [ -z "$py_label" ]; then
  echo "FAIL: cannot read COVERAGE_OVERRIDE_LABEL from cli/src/fno/pr/_reviews.py" >&2
  exit 1
fi

# The context: what GitHub requires (ruleset, exact list membership), who
# writes it (Python, Rust, workflow), and who reads it back (audit).
if ! python3 - "$ROOT/scripts/ci/merge-ruleset.json" "$py_ctx" <<'EOF'
import json, sys

d = json.load(open(sys.argv[1]))
want = sys.argv[2]
rsc = next((r for r in d.get("rules", []) if r.get("type") == "required_status_checks"), None)
checks = (rsc or {}).get("parameters", {}).get("required_status_checks") or []
contexts = [str(c.get("context")) for c in checks if isinstance(c, dict)]
sys.exit(0 if want in contexts else 1)
EOF
then
  mismatch "the ruleset data does not require exactly '$py_ctx'"
fi
expect_fixed "$ROOT/crates/fno-agents/src/loopcheck.rs" \
  "const COVERAGE_STATUS_CONTEXT: &str = \"$py_ctx\";" "the Rust publisher"
expect_line "$ROOT/.github/workflows/review-coverage-gate.yml" \
  "^[[:space:]]*ctx=$py_ctx\$" "the refresher workflow"
expect_line "$ROOT/scripts/ci/check-merge-coverage-audit.sh" \
  "^CTX=\"$py_ctx\"\$" "the post-merge audit"

# The override label: the 3am release valve, spelled by every writer and by
# the audit that counts its use by name. The Rust needle includes the jq
# quoting because that inline string is the label's only Rust spelling; the
# audit needle includes the case-pattern close so a suffixed rename misses.
expect_fixed "$ROOT/crates/fno-agents/src/loopcheck.rs" \
  "index(\\\"$py_label\\\")" "the Rust publisher (override label)"
expect_fixed "$ROOT/.github/workflows/review-coverage-gate.yml" \
  '"$LABEL" = "'"$py_label"'"' "the refresher workflow (override label)"
# The withdrawal arm's case pattern, not just the labeled arm's compare: a
# rename that updates every other pinned surface but leaves this arm falling
# to `*)` keeps parity green while a withdrawn override never invalidates its
# own green - the valve stays open with every check passing.
expect_fixed "$ROOT/.github/workflows/review-coverage-gate.yml" \
  "${py_label}*)" "the refresher withdrawal arm (override description prefix)"
expect_fixed "$ROOT/scripts/ci/check-merge-coverage-audit.sh" \
  "${py_label}*)" "the post-merge audit (override label)"

# The description grammar: the refresher's invalidate arms switch on prose
# prefixes the publishers emit (the preserve list in the workflow), so those
# prefixes are an ABI like the context and the label - a wording change on
# any writer side would make the next push clobber a green verdict while
# every other check stays green. Each needle anchors at the CONSTRUCTION
# site (f"covered / format!("covered), stable across renames inside the
# braces) so a variable rename cannot false-red the pin.
expect_fixed "$ROOT/cli/src/fno/pr/_reviews.py" \
  'f"covered' "the Python publisher (covered description prefix)"
expect_fixed "$ROOT/cli/src/fno/pr/_reviews.py" \
  '"no review lane configured; merge ungated"' "the Python publisher (no-lane description)"
expect_fixed "$ROOT/crates/fno-agents/src/loopcheck.rs" \
  'format!("covered' "the Rust publisher (covered description prefix)"
expect_line "$ROOT/.github/workflows/review-coverage-gate.yml" \
  'covered\*\|"no review lane"\*' "the refresher preserve list"

if [ "$fail" = 1 ]; then
  echo "FAIL: coverage context/label drift - the messages above name the surfaces" >&2
  exit 1
fi
echo "ok: context '$py_ctx' and label '$py_label' agree on every surface"

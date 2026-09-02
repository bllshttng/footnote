#!/usr/bin/env bash
# Positive control for `fno doctor lint seam-crossings`.
#
# A green checker aimed at nothing still reads as proof. This harness builds a
# minimal fixture tree and proves each of the three assertions FAILS on
# injected drift, then PASSES once the injection is removed - the same
# injected-drift discipline as tests/ci/test_provider_vocabulary_parity.sh.
# The helper-discovery case is the load-bearing one: a NEW resolver helper
# under a fresh name, with no literal `Command::new("fno")` anywhere, must
# fail the check on the helper's SHAPE alone.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

tmp=$(mktemp -d "${TMPDIR:-/tmp}/seam-crossings.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/crates/probe/src" "$tmp/cli/src/fno" "$tmp/scripts/ci"

# The fixture: one env-key resolver (a helper spelling), one literal launch,
# a comment mentioning the idiom (must never count), and a cfg(test) block
# whose launch must never count either.
cat > "$tmp/crates/probe/src/lib.rs" <<'RS'
use std::process::Command;

fn probe_fno_bin() -> String {
    std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string())
}

fn launch() {
    let mut cmd = Command::new("fno");
    cmd.arg("doctor");
}

// a comment mentioning Command::new("fno") is prose about the idiom

#[cfg(test)]
mod tests {
    #[test]
    fn never_counts() {
        let mut cmd = Command::new("fno");
    }
}
RS

cat > "$tmp/cli/src/fno/rust_binary.py" <<'PY'
"""The single door. This file is exempt from the pydoor rule."""
PY

cat > "$tmp/cli/src/fno/innocent.py" <<'PY'
import subprocess


def ping() -> None:
    subprocess.run(["fno", "agents", "ping"], check=False)
PY

status=0
note() { printf '%s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; status=1; }

seed_baseline() {
  (cd "$ROOT/cli" && uv run python - "$tmp" <<'PYEOF'
import sys
from pathlib import Path

from fno import lint_seam_crossings as lsc

root = Path(sys.argv[1])
(root / lsc.BASELINE_RELPATH).write_text(lsc.build_baseline(root), encoding="utf-8")
PYEOF
)
}

check_clean() {
  (cd "$ROOT/cli" && uv run python - "$tmp" <<'PYEOF'
import sys
from pathlib import Path

from fno import lint_seam_crossings as lsc

sys.exit(lsc.run(Path(sys.argv[1])))
PYEOF
)
}

# seed: regenerate the fixture baseline from the clean tree, then require a
# clean pass AND a non-empty baseline (a green control aimed at nothing is
# not a control).
if ! seed_baseline; then
  fail "baseline seeding crashed"
fi

baseline_sites=$(grep -c -v '^#' "$tmp/scripts/ci/seam-crossings-baseline.txt")
if [[ "${baseline_sites:-0}" -lt 2 ]]; then
  fail "fixture baseline holds $baseline_sites site(s); the control must aim at real sites"
fi
note "PASS: fixture baseline seeded ($baseline_sites site(s))"

cp "$tmp/crates/probe/src/lib.rs" "$tmp/lib.rs.clean"
cp "$tmp/cli/src/fno/innocent.py" "$tmp/innocent.py.clean"

if out=$(check_clean 2>&1); then
  note "PASS: clean fixture accepted"
else
  printf '%s\n' "$out" >&2
  fail "clean fixture rejected"
fi

# case 1: a NEW literal crossing fails, naming the file.
cat >> "$tmp/crates/probe/src/lib.rs" <<'RS'

fn added() {
    let mut cmd = Command::new("fno");
}
RS
if out=$(check_clean 2>&1); then
  fail 'added Command::new("fno") accepted'
else
  case "$out" in
    *probe/src/lib.rs*) note "PASS: added crossing rejected, file named" ;;
    *) printf '%s\n' "$out" >&2; fail "added crossing rejected without naming the file" ;;
  esac
fi
cp "$tmp/lib.rs.clean" "$tmp/crates/probe/src/lib.rs"

# case 2: a baselined site removed from source (baseline not regenerated)
# fails - the both-directional half. The strip drops every literal-launch
# line, production and cfg(test) alike; the comment line quoting the idiom
# stays, but a comment never was a site.
grep -v 'Command::new("fno")' "$tmp/lib.rs.clean" > "$tmp/crates/probe/src/lib.rs"
if out=$(check_clean 2>&1); then
  fail "removed baselined site accepted (ratchet does not fail both directions)"
else
  case "$out" in
    *no*longer*matches*) note "PASS: baselined-but-removed site rejected" ;;
    *) printf '%s\n' "$out" >&2; fail "removed-site failure does not name the stale line" ;;
  esac
fi
cp "$tmp/lib.rs.clean" "$tmp/crates/probe/src/lib.rs"

# case 3 (the load-bearing one): a NEW resolver helper under a fresh name,
# plus a call to it - no literal Command::new anywhere - must fail on shape.
cat >> "$tmp/crates/probe/src/lib.rs" <<'RS'

fn porcelain_path() -> String {
    std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string())
}

fn uses_helper() {
    let mut cmd = Command::new(porcelain_path());
}
RS
if out=$(check_clean 2>&1); then
  fail "new resolver helper under an unbaselined name accepted"
else
  case "$out" in
    *porcelain_path*) note "PASS: unbaselined resolver helper rejected, definition named" ;;
    *) printf '%s\n' "$out" >&2; fail "helper-discovery failure does not name the definition" ;;
  esac
fi
cp "$tmp/lib.rs.clean" "$tmp/crates/probe/src/lib.rs"

# the call line is invisible to the crossing rule (no literal), proving the
# previous failure came from the resolver SHAPE, not a crossing-count rise.
if ! (cd "$ROOT/cli" && uv run python - "$tmp" <<'PYEOF'
import sys
from pathlib import Path

from fno import lint_seam_crossings as lsc

root = Path(sys.argv[1])
sites = lsc.enumerate_sites(root)
literal_new = [
    t for _r, _l, t in sites[lsc.SITE_RULE] if 'Command::new("fno")' in t
]
# exactly the ONE original literal, never more, helper call included
sys.exit(0 if len(literal_new) == 1 else 1)
PYEOF
); then
  fail "crossing count rose with the helper injection; discovery case is not isolated"
else
  note "PASS: helper case failed on shape with no crossing-count rise"
fi

# case 4: a Python file outside the door execing literal fno-agents fails,
# in BOTH shapes: the inline argv list, and an argv assembled into a
# variable (the shape a call-line window misses).
cat >> "$tmp/cli/src/fno/innocent.py" <<'PY'


def agents_ping() -> None:
    subprocess.run(["fno-agents", "agents", "ping"], check=False)
PY
if out=$(check_clean 2>&1); then
  fail "literal fno-agents exec outside rust_binary.py accepted"
else
  case "$out" in
    *innocent.py*) note "PASS: pydoor violation rejected, file named" ;;
    *) printf '%s\n' "$out" >&2; fail "pydoor failure does not name the file" ;;
  esac
fi
cp "$tmp/innocent.py.clean" "$tmp/cli/src/fno/innocent.py"

cat >> "$tmp/cli/src/fno/innocent.py" <<'PY'


def assembled_ping() -> None:
    argv = ["fno-agents", "agents", "ping"]
    subprocess.run(argv, check=False)
PY
if out=$(check_clean 2>&1); then
  fail "variable-assembled fno-agents argv accepted"
else
  case "$out" in
    *innocent.py*) note "PASS: variable-assembled argv rejected, file named" ;;
    *) printf '%s\n' "$out" >&2; fail "assembled-argv failure does not name the file" ;;
  esac
fi
cp "$tmp/innocent.py.clean" "$tmp/cli/src/fno/innocent.py"

# case 4c: a LOCATOR is not an exec. `shutil.which("fno-agents")` carries the
# literal as a call argument; the pydoor rule must stay green on it so the
# argv0 shape never widens into ratcheting every mention.
cat >> "$tmp/cli/src/fno/innocent.py" <<'PY'


def locate() -> str:
    import shutil

    return shutil.which("fno-agents") or "fno-agents"
PY
if out=$(check_clean 2>&1); then
  note "PASS: locator mention stays green (not an exec)"
else
  printf '%s\n' "$out" >&2
  fail "pydoor ratcheted a locator call"
fi
cp "$tmp/innocent.py.clean" "$tmp/cli/src/fno/innocent.py"

# case 5: a line MOVE above a baselined site must NOT churn the baseline -
# matching keys on content, never on the line number.
sed '1a\
\
// inserted above the resolver: every line number below shifts by one
' "$tmp/lib.rs.clean" > "$tmp/crates/probe/src/lib.rs"
if out=$(check_clean 2>&1); then
  note "PASS: line move above baselined sites stays green"
else
  printf '%s\n' "$out" >&2
  fail "line-key churn: a comment insertion forced a regeneration"
fi
cp "$tmp/lib.rs.clean" "$tmp/crates/probe/src/lib.rs"

# final state must be clean again (fixture restored, baseline untouched)
if out=$(check_clean 2>&1); then
  note "PASS: fixture restored to green"
else
  printf '%s\n' "$out" >&2
  fail "fixture did not return to green"
fi

if [[ "$status" -ne 0 ]]; then
  echo "seam crossings test: FAILED" >&2
  exit 1
fi
echo "seam crossings test: all cases passed"

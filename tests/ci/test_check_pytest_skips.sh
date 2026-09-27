#!/usr/bin/env bash
# tests/ci/test_check_pytest_skips.sh
#
# Exercises scripts/ci/check-pytest-skips.sh against scratch git repos: each
# case builds a tree, writes a baseline, and asserts the exit code AND a line
# only that outcome prints, so a gate that skipped a path cannot pass by
# printing nothing.
#
# Run: bash tests/ci/test_check_pytest_skips.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$(cd "${SCRIPT_DIR}/../.." && pwd)/scripts/ci/check-pytest-skips.sh"
[[ -f "$GATE" ]] || { echo "gate not found at $GATE" >&2; exit 1; }

# The caller's git config (signing, hooks) must not reach the scratch repos.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PASS=0; FAIL=0

# fresh: a scratch repo holding a header-only baseline, ready for a case.
fresh() {
  rm -rf "$TMP/repo"
  git init -q "$TMP/repo"
  cd "$TMP/repo" || exit 1
  mkdir -p scripts/ci tests
  printf '%s\n' "# Known pytest skip sites, held by the CI ratchet." > scripts/ci/pytest-skips-baseline.txt
  git add -A && git commit -qm base
}

# baseline <key>...: append reason-carrying lines.
baseline() {
  local key
  for key in "$@"; do
    printf '%s  # held: pre-existing on main\n' "$key" >> scripts/ci/pytest-skips-baseline.txt
  done
}

commit() { git add -A && git commit -qm change; }

# check <label> <want_exit> <marker>
check() {
  local label="$1" want="$2" marker="$3" out got
  out="$(bash "$GATE" 2>&1)"; got=$?
  if [[ "$got" -eq "$want" && "$out" == *"$marker"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want exit %s with: %s\n  got exit %s:\n%s\n' "$label" "$want" "$marker" "$got" "$out"
  fi
}

# --- the happy paths ----------------------------------------------------------
fresh
check 'a tree with no skip sites matches a header-only baseline' 0 'ok (0 site(s) match the baseline)'

fresh
cat > tests/test_a.py <<'EOF'
import pytest

@pytest.mark.skip(reason="wip")
def test_one():
    assert True
EOF
baseline 'tests/test_a.py::test_one::mark.skip'
commit
check 'a skip whose baseline line carries a reason passes' 0 'site(s) match the baseline'

fresh
printf 'def broken(:\n' > tests/broken.py
cat > tests/test_f.py <<'EOF'
import pytest

@pytest.mark.skip(reason="wip")
def test_six():
    assert True
EOF
baseline 'tests/test_f.py::test_six::mark.skip'
commit
check 'an unparseable file is named and does not fail the run' 0 'skipping unparseable file tests/broken.py'

# --- the refusals -------------------------------------------------------------
fresh
cat > tests/test_a.py <<'EOF'
import pytest

@pytest.mark.skip(reason="wip")
def test_one():
    assert True
EOF
commit
check 'an added skip with no baseline line is refused' 1 'tests/test_a.py::test_one::mark.skip'

fresh
cat > tests/test_gone.py <<'EOF'
def test_old():
    assert True
EOF
baseline 'tests/test_old.py::test_old::mark.skip'
commit
check 'a baseline line whose site left the tree is refused' 1 'no longer in the tree'

# --- the baseline's own rule --------------------------------------------------
fresh
printf '%s\n' 'tests/test_a.py::test_one::mark.skip' >> scripts/ci/pytest-skips-baseline.txt
commit
check 'a baseline line with no reason is refused naming the line' 1 'baseline line 2 has no reason'

# --- alias and form resolution ------------------------------------------------
fresh
cat > tests/test_b.py <<'EOF'
import pytest as _pytest

def test_two():
    _pytest.skip("later")
EOF
commit
check 'an aliased pytest import resolves to the canonical kind' 1 'tests/test_b.py::test_two::pytest.skip'

fresh
cat > tests/test_c.py <<'EOF'
from pytest import mark

@mark.skipif(True, reason="env")
def test_three():
    assert True
EOF
commit
check 'a mark imported from pytest resolves to mark.skipif' 1 'tests/test_c.py::test_three::mark.skipif'

fresh
cat > tests/test_d.py <<'EOF'
import pytest

pytestmark = pytest.mark.skipif(True, reason="env")

def test_four():
    assert True
EOF
commit
check 'a module-level pytestmark is keyed <module>' 1 'tests/test_d.py::<module>::mark.skipif'

# --- reusable marker applications ---------------------------------------------
fresh
cat > tests/test_g.py <<'EOF'
import pytest

requires_x = pytest.mark.skipif(True, reason="env")

@requires_x
def test_seven():
    assert True
EOF
baseline 'tests/test_g.py::<module>::mark.skipif'
commit
check 'an existing marker pinned on a test is a new site even when only the definition is baselined' 1 'tests/test_g.py::test_seven::marker.requires_x'

fresh
cat > tests/test_g.py <<'EOF'
import pytest

requires_x = pytest.mark.skipif(True, reason="env")

@requires_x
def test_seven():
    assert True
EOF
baseline 'tests/test_g.py::<module>::mark.skipif' 'tests/test_g.py::test_seven::marker.requires_x'
commit
check 'a marker application whose baseline line carries a reason passes' 0 'site(s) match the baseline'

fresh
cat > tests/test_h.py <<'EOF'
import pytest

requires_y = pytest.mark.skipif(True, reason="env")

pytestmark = requires_y

def test_eight():
    assert True
EOF
baseline 'tests/test_h.py::<module>::mark.skipif'
commit
check 'a module-wide marker application via pytestmark is a new site' 1 'tests/test_h.py::<module>::marker.requires_y'

# --- the multiset rule --------------------------------------------------------
fresh
cat > tests/test_e.py <<'EOF'
import pytest

def test_five():
    pytest.skip("first")
    pytest.skip("second")
EOF
baseline 'tests/test_e.py::test_five::pytest.skip'
commit
check 'a second skip in a test that already has one is a new site' 1 'NEW pytest skip site(s)'

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]

#!/usr/bin/env bash
# test-setup-worktree-code-indexes.sh - checkout-local provisioning of the
# graphify and codegraph indexes in scripts/setup/setup-worktree.sh.
#
# The two indexes must never be shared with canonical: `graphify update .`
# inside a worktree would write branch content into the canonical graph, and
# the codegraph daemon watches exactly one checkout. graphify-out is seeded
# by a copy (APFS clone first, deep copy where cloning is unsupported);
# .codegraph is initialized fresh from the worktree's own source.
#
# Tests (stub `codegraph` on PATH; /bin tools stay real - the setup script
# prepends /usr/bin:/bin to PATH, so a cp or fno stub could not work anyway,
# and PATH is pinned to the system dirs so a locally installed real codegraph
# or fno can never answer):
#  T1 fresh canonical indexes -> graphify-out copied as a real dir, codegraph
#     init -y invoked for the exact worktree, canonical bytes untouched.
#  T2 absent canonical indexes -> named skips, nothing created, exit 0.
#  T3 existing real targets -> preserved byte-for-byte, stub not invoked.
#  T4 existing symlink targets -> refused, not followed, exit 0.
#  T5 indexer failure -> warning, exit 0, no .codegraph, graphify still copied.
#  T6 codegraph CLI absent -> named skip, exit 0.
#  T7 repo .gitignore covers root + nested graphify-out and .codegraph.
#
# Bash 3.2 compatible; hermetic mktemp sandboxes.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$REPO_ROOT/scripts/setup/setup-worktree.sh"

# The setup script prepends /usr/bin:/bin itself; starting from the system
# dirs keeps `codegraph` and `fno` out of every invocation unless the stub
# dir is on the path.
SYS_PATH="/usr/bin:/bin"

pass=0
fail=0

check_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (expected='$expected' actual='$actual')"
    fail=$((fail+1))
  fi
}

check_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (needle='$needle' not found in: $haystack)"
    fail=$((fail+1))
  fi
}

check_true() {
  # desc, then a command whose SUCCESS is the assertion
  local desc="$1"
  shift
  if "$@"; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc"
    fail=$((fail+1))
  fi
}

# Stub codegraph: logs argv on one line, fakes a fresh index in the worktree
# it was handed (arg 3 of `codegraph init -y <worktree>`). The log's presence
# is the positive marker that provisioning ran; its content asserts args.
make_stub_bin() {
  local dir="$1" log="$2"
  mkdir -p "$dir"
  {
    printf '#!/usr/bin/env bash\n'
    printf 'printf "%%s\\n" "$*" >> "%s"\n' "$log"
    printf 'if [ -n "${CODEGRAPH_STUB_FAIL:-}" ]; then exit 1; fi\n'
    printf 'wt="$3"\n'
    printf 'mkdir -p "$wt/.codegraph"\n'
    printf 'printf stub-db > "$wt/.codegraph/db"\n'
  } > "$dir/codegraph"
  chmod +x "$dir/codegraph"
}

# A canonical checkout with both indexes: a small graphify-out dir and the
# symlinked .codegraph layout older installs use.
make_canonical_with_indexes() {
  local root="$1"
  mkdir -p "$root/graphify-out" "$root/fake-codegraph-home"
  printf 'GRAPH-BYTES\n' > "$root/graphify-out/graph.json"
  printf 'REPORT\n' > "$root/graphify-out/GRAPH_REPORT.md"
  printf 'DB\n' > "$root/fake-codegraph-home/codegraph.db"
  ln -s "$root/fake-codegraph-home" "$root/.codegraph"
}

# ---------------------------------------------------------------------------
# T1: fresh canonical indexes - copy + init, canonical bytes untouched
# ---------------------------------------------------------------------------
SBX1="$(mktemp -d)"
make_canonical_with_indexes "$SBX1/canonical"
mkdir -p "$SBX1/worktree"
STUB1="$SBX1/stub-bin"
LOG1="$SBX1/stub.log"
make_stub_bin "$STUB1" "$LOG1"
BEFORE1="$(cat "$SBX1/canonical/graphify-out/graph.json")"
OUT1="$(PATH="$SYS_PATH:$STUB1" CANONICAL="$SBX1/canonical" WORKTREE="$SBX1/worktree" bash "$SETUP" 2>&1)"
RC1=$?

check_eq "T1: exits 0" "0" "$RC1"
check_true "T1: graphify-out is a real dir, not a symlink" test -d "$SBX1/worktree/graphify-out"
check_true "T1: graphify-out is NOT a symlink" test ! -L "$SBX1/worktree/graphify-out"
check_eq "T1: graphify-out content was copied" "$BEFORE1" "$(cat "$SBX1/worktree/graphify-out/graph.json")"
check_contains "T1: graphify copy receipt" "graphify-out: copied" "$OUT1"
check_eq "T1: codegraph stub invoked with init -y and the exact worktree" \
  "init -y $SBX1/worktree" "$(cat "$LOG1" 2>/dev/null)"
check_true "T1: .codegraph exists in the worktree" test -e "$SBX1/worktree/.codegraph"
check_true "T1: .codegraph is worktree-owned, not a symlink" test ! -L "$SBX1/worktree/.codegraph"
check_eq "T1: canonical graph bytes survive" "$BEFORE1" "$(cat "$SBX1/canonical/graphify-out/graph.json")"
check_true "T1: canonical .codegraph symlink untouched" test -L "$SBX1/canonical/.codegraph"
printf 'WORKTREE-MUTATION\n' > "$SBX1/worktree/graphify-out/graph.json"
check_eq "T1: mutating the worktree copy cannot reach canonical bytes" \
  "$BEFORE1" "$(cat "$SBX1/canonical/graphify-out/graph.json")"

# ---------------------------------------------------------------------------
# T2: absent canonical indexes - named skips, nothing created, exit 0
# ---------------------------------------------------------------------------
SBX2="$(mktemp -d)"
mkdir -p "$SBX2/canonical" "$SBX2/worktree"
OUT2="$(PATH="$SYS_PATH" CANONICAL="$SBX2/canonical" WORKTREE="$SBX2/worktree" bash "$SETUP" 2>&1)"
RC2=$?

check_eq "T2: exits 0 with neither index present" "0" "$RC2"
check_contains "T2: graphify skip is named" "graphify-out: canonical index missing" "$OUT2"
check_contains "T2: codegraph skip is named" "codegraph: canonical index missing" "$OUT2"
check_true "T2: no graphify-out created" test ! -e "$SBX2/worktree/graphify-out"
check_true "T2: no .codegraph created" test ! -e "$SBX2/worktree/.codegraph"

# ---------------------------------------------------------------------------
# T3: existing real targets - preserved byte-for-byte, stub never invoked
# ---------------------------------------------------------------------------
SBX3="$(mktemp -d)"
make_canonical_with_indexes "$SBX3/canonical"
mkdir -p "$SBX3/worktree/graphify-out" "$SBX3/worktree/.codegraph"
printf 'WORKTREE-OWN\n' > "$SBX3/worktree/graphify-out/graph.json"
printf 'WORKTREE-DB\n' > "$SBX3/worktree/.codegraph/db"
STUB3="$SBX3/stub-bin"
LOG3="$SBX3/stub.log"
make_stub_bin "$STUB3" "$LOG3"
OUT3="$(PATH="$SYS_PATH:$STUB3" CANONICAL="$SBX3/canonical" WORKTREE="$SBX3/worktree" bash "$SETUP" 2>&1)"
RC3=$?

check_eq "T3: exits 0" "0" "$RC3"
check_eq "T3: existing graphify-out keeps its bytes" \
  "WORKTREE-OWN" "$(cat "$SBX3/worktree/graphify-out/graph.json")"
check_eq "T3: existing .codegraph keeps its bytes" \
  "WORKTREE-DB" "$(cat "$SBX3/worktree/.codegraph/db")"
check_contains "T3: graphify preserve receipt" "graphify-out: already present" "$OUT3"
check_true "T3: codegraph stub never ran" test ! -f "$LOG3"

# ---------------------------------------------------------------------------
# T4: symlink targets - refused, not followed, exit 0
# ---------------------------------------------------------------------------
SBX4="$(mktemp -d)"
make_canonical_with_indexes "$SBX4/canonical"
mkdir -p "$SBX4/worktree" "$SBX4/dummy-g" "$SBX4/dummy-c"
ln -s "$SBX4/dummy-g" "$SBX4/worktree/graphify-out"
ln -s "$SBX4/dummy-c" "$SBX4/worktree/.codegraph"
STUB4="$SBX4/stub-bin"
LOG4="$SBX4/stub.log"
make_stub_bin "$STUB4" "$LOG4"
OUT4="$(PATH="$SYS_PATH:$STUB4" CANONICAL="$SBX4/canonical" WORKTREE="$SBX4/worktree" bash "$SETUP" 2>&1)"
RC4=$?

check_eq "T4: exits 0" "0" "$RC4"
check_contains "T4: graphify symlink refusal named" "graphify-out: refusing symlink" "$OUT4"
check_contains "T4: codegraph symlink refusal named" "codegraph: refusing symlink" "$OUT4"
check_eq "T4: graphify symlink still points at the dummy" "$SBX4/dummy-g" "$(readlink "$SBX4/worktree/graphify-out")"
check_eq "T4: codegraph symlink still points at the dummy" "$SBX4/dummy-c" "$(readlink "$SBX4/worktree/.codegraph")"
check_eq "T4: the dummy targets were not followed into" "0" "$(find "$SBX4/dummy-g" "$SBX4/dummy-c" -mindepth 1 2>/dev/null | wc -l | tr -d ' ')"
check_true "T4: codegraph stub never ran" test ! -f "$LOG4"

# ---------------------------------------------------------------------------
# T5: indexer failure - warning, exit 0, graphify still provisioned
# ---------------------------------------------------------------------------
SBX5="$(mktemp -d)"
make_canonical_with_indexes "$SBX5/canonical"
mkdir -p "$SBX5/worktree"
STUB5="$SBX5/stub-bin"
LOG5="$SBX5/stub.log"
make_stub_bin "$STUB5" "$LOG5"
OUT5="$(PATH="$SYS_PATH:$STUB5" CODEGRAPH_STUB_FAIL=1 CANONICAL="$SBX5/canonical" WORKTREE="$SBX5/worktree" bash "$SETUP" 2>&1)"
RC5=$?

check_eq "T5: exits 0 despite the indexer failing" "0" "$RC5"
check_contains "T5: indexer failure is named" "codegraph: init failed" "$OUT5"
check_true "T5: no .codegraph left behind" test ! -e "$SBX5/worktree/.codegraph"
check_true "T5: graphify-out still copied" test -d "$SBX5/worktree/graphify-out"

# ---------------------------------------------------------------------------
# T6: codegraph CLI absent - named skip, exit 0
# ---------------------------------------------------------------------------
SBX6="$(mktemp -d)"
make_canonical_with_indexes "$SBX6/canonical"
mkdir -p "$SBX6/worktree"
OUT6="$(PATH="$SYS_PATH" CANONICAL="$SBX6/canonical" WORKTREE="$SBX6/worktree" bash "$SETUP" 2>&1)"
RC6=$?

check_eq "T6: exits 0 without the CLI" "0" "$RC6"
check_contains "T6: CLI skip is named" "codegraph: CLI missing" "$OUT6"
check_true "T6: no .codegraph created" test ! -e "$SBX6/worktree/.codegraph"
check_true "T6: graphify-out still copied" test -d "$SBX6/worktree/graphify-out"

# ---------------------------------------------------------------------------
# T7: repo .gitignore covers root and nested graphify-out, plus .codegraph
# ---------------------------------------------------------------------------
# Trailing slashes on the dir probes: a dir-only ignore pattern (`foo/`) never
# matches a bare nonexistent name, and existence must not gate this probe.
IGNORE_HITS="$(git -C "$REPO_ROOT" check-ignore -v graphify-out/ cli/graphify-out/ docs/graphify-out/ .codegraph 2>/dev/null | wc -l | tr -d ' ')"
check_eq "T7: check-ignore matches all four probe paths" "4" "$IGNORE_HITS"
STATUS7="$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all 2>/dev/null)"
if printf '%s' "$STATUS7" | grep -q 'graphify-out'; then
  echo "FAIL: T7: a graphify-out artifact leaked into git status"
  fail=$((fail+1))
else
  echo "PASS: T7: no graphify-out artifact in git status"
  pass=$((pass+1))
fi
if printf '%s' "$STATUS7" | grep -q '\.codegraph'; then
  echo "FAIL: T7: a .codegraph artifact leaked into git status"
  fail=$((fail+1))
else
  echo "PASS: T7: no .codegraph artifact in git status"
  pass=$((pass+1))
fi

rm -rf "$SBX1" "$SBX2" "$SBX3" "$SBX4" "$SBX5" "$SBX6"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]

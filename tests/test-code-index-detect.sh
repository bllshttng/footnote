#!/usr/bin/env bash
# test-code-index-detect.sh - the provider detection reader
# (scripts/lib/code-index-detect.sh).
#
# Covered:
#  AC1-HP  both bundled manifests + a repo holding .codegraph and
#          graphify-out/graph.json with both CLIs on PATH -> one ready line
#          each, exit 0.
#  AC1-EDGE  a user manifest in <repo>/.fno/code-index/providers/ with the
#          same name wins: its roles and its path print, no second line.
#  AC2-ERR  a manifest with no detect line, or a name with an uppercase
#          letter, is skipped by name on stderr; the others still print;
#          exit 0.
#  AC2-EDGE  .codegraph present and no codegraph on PATH -> the line reads
#          unavailable:requires codegraph not on PATH.
#
# Hermetic: every run gets a throwaway HOME and a system-only PATH, so the
# real ~/.fno providers and CLIs can never answer. Bash 3.2 compatible.

set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DETECT="$REPO_ROOT/scripts/lib/code-index-detect.sh"
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
  if printf '%s' "$haystack" | grep -qF -- "$needle"; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (needle='$needle' not found)"
    fail=$((fail+1))
  fi
}

check_true() {
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

# A stub CLI whose existence is the only thing detection checks: it never
# runs ask, fresh or refresh.
make_stub_bin() {
  local dir="$1" cmd="$2"
  mkdir -p "$dir"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$dir/$cmd"
  chmod +x "$dir/$cmd"
}

# A fixture repo holding both bundled detect paths.
make_repo_with_indexes() {
  local root="$1"
  mkdir -p "$root/.codegraph" "$root/graphify-out"
  printf 'stub\n' > "$root/.codegraph/db"
  printf '{}\n' > "$root/graphify-out/graph.json"
}

write_manifest() {
  local path="$1" name="$2" roles="$3" detect="$4" requires="$5" extra="$6"
  mkdir -p "$(dirname "$path")"
  {
    printf 'schema = 1\n'
    printf 'name = "%s"\n' "$name"
    printf 'roles = %s\n' "$roles"
    if [ -n "$detect" ]; then printf 'detect = "%s"\n' "$detect"; fi
    if [ -n "$requires" ]; then printf 'requires = "%s"\n' "$requires"; fi
    if [ -n "$extra" ]; then printf '%s\n' "$extra"; fi
  } > "$path"
}

# ---------------------------------------------------------------------------
# AC1-HP: both indexes present, both CLIs on PATH -> two ready lines, exit 0
# ---------------------------------------------------------------------------
SBX1="$(mktemp -d)"
mkdir -p "$SBX1/home"
make_repo_with_indexes "$SBX1/repo"
STUB1="$SBX1/stub-bin"
make_stub_bin "$STUB1" codegraph
make_stub_bin "$STUB1" graphify
OUT1="$(PATH="$SYS_PATH:$STUB1" HOME="$SBX1/home" bash "$DETECT" "$SBX1/repo" 2>"$SBX1/err")"
RC1=$?
CG1="$(printf '%s\n' "$OUT1" | grep -c $'^codegraph\t' || true)"
GR1="$(printf '%s\n' "$OUT1" | grep -c $'^graphify\t' || true)"
check_eq "AC1-HP: exits 0" "0" "$RC1"
check_eq "AC1-HP: exactly one codegraph line" "1" "$CG1"
check_eq "AC1-HP: exactly one graphify line" "1" "$GR1"
check_contains "AC1-HP: codegraph is ready with role symbol" \
  "$(printf 'codegraph\tsymbol\tready\t')" "$OUT1"
check_contains "AC1-HP: graphify is ready with role semantic" \
  "$(printf 'graphify\tsemantic\tready\t')" "$OUT1"
check_contains "AC1-HP: codegraph names its bundled manifest" \
  "skills/blueprint/code-index/providers/codegraph.toml" "$OUT1"
check_true "AC1-HP: nothing on stderr" test ! -s "$SBX1/err"

# ---------------------------------------------------------------------------
# AC1-EDGE: a user manifest with the same name wins
# ---------------------------------------------------------------------------
USER1="$SBX1/repo/.fno/code-index/providers/codegraph.toml"
write_manifest "$USER1" codegraph '["symbol","semantic"]' ".codegraph" "codegraph" ""
OUT2="$(PATH="$SYS_PATH:$STUB1" HOME="$SBX1/home" bash "$DETECT" "$SBX1/repo" 2>/dev/null)"
CG2="$(printf '%s\n' "$OUT2" | grep $'^codegraph\t')"
CG2N="$(printf '%s\n' "$CG2" | grep -c $'^codegraph\t' || true)"
check_eq "AC1-EDGE: exactly one codegraph line" "1" "$CG2N"
check_contains "AC1-EDGE: the line carries the user roles" \
  "$(printf 'codegraph\tsymbol,semantic\t')" "$OUT2"
check_contains "AC1-EDGE: the line names the repo manifest path" \
  "$SBX1/repo/.fno/code-index/providers/codegraph.toml" "$CG2"

# ---------------------------------------------------------------------------
# AC2-ERR: malformed manifests are skipped by name; the others still print
# ---------------------------------------------------------------------------
BAD1="$SBX1/repo/.fno/code-index/providers/bad1.toml"
write_manifest "$BAD1" broken '["symbol"]' '' "codegraph" ""
BAD2="$SBX1/repo/.fno/code-index/providers/bad2.toml"
write_manifest "$BAD2" "CodeGraph" '["symbol"]' ".codegraph" "codegraph" ""
OUT3="$(PATH="$SYS_PATH:$STUB1" HOME="$SBX1/home" bash "$DETECT" "$SBX1/repo" 2>"$SBX1/err3")"
RC3=$?
check_eq "AC2-ERR: exits 0 despite malformed manifests" "0" "$RC3"
check_contains "AC2-ERR: the no-detect manifest is skipped by path" \
  "code-index: skipped $BAD1: no detect line" "$(cat "$SBX1/err3")"
check_contains "AC2-ERR: the uppercase name is skipped by path" \
  "code-index: skipped $BAD2: bad name 'CodeGraph'" "$(cat "$SBX1/err3")"
check_true "AC2-ERR: the good providers still print" \
  grep -q $'^codegraph\t' <<< "$OUT3"
check_true "AC2-ERR: graphify still prints" \
  grep -q $'^graphify\t' <<< "$OUT3"
check_true "AC2-ERR: malformed manifests emit no stdout line" \
  test -z "$(printf '%s\n' "$OUT3" | grep -E $'^(broken|CodeGraph)\t' || true)"

# ---------------------------------------------------------------------------
# AC2-EDGE: .codegraph present, no codegraph on PATH -> unavailable line
# ---------------------------------------------------------------------------
STUB4="$SBX1/stub-graphify-only"
make_stub_bin "$STUB4" graphify
OUT4="$(PATH="$SYS_PATH:$STUB4" HOME="$SBX1/home" bash "$DETECT" "$SBX1/repo" 2>/dev/null)"
check_contains "AC2-EDGE: codegraph reads unavailable:requires codegraph not on PATH" \
  "$(printf 'codegraph\tsymbol,semantic\tunavailable:requires codegraph not on PATH\t')" "$OUT4"

rm -rf "$SBX1"

# ---------------------------------------------------------------------------
# AC2-SHADOW: a same-name override whose index is absent still shadows the
# bundled provider - presence is checked after the name wins, never before.
# ---------------------------------------------------------------------------
SBX2="$(mktemp -d)"
mkdir -p "$SBX2/home" "$SBX2/repo/.fno/code-index/providers"
write_manifest "$SBX2/repo/.fno/code-index/providers/codegraph.toml" codegraph '["symbol"]' "missing-index/marker" "codegraph" ""
OUT5="$(PATH="$SYS_PATH" HOME="$SBX2/home" bash "$DETECT" "$SBX2/repo" 2>"$SBX2/err5")"
RC5=$?
check_eq "AC2-SHADOW: exits 0" "0" "$RC5"
check_true "AC2-SHADOW: no codegraph line prints from the bundled fallback" \
  test -z "$(printf '%s\n' "$OUT5" | grep $'^codegraph\t' || true)"
check_true "AC2-SHADOW: the override leaves no error either" test ! -s "$SBX2/err5"

rm -rf "$SBX2"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]

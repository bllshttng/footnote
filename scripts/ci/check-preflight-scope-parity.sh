#!/usr/bin/env bash
# The preflight receipt scope is declared in FOUR places, and every one of them
# must name the same legs.
#
# Why a gate and not a comment. `_trusted_preflight_producer` was widened in
# Python to accept a new optional leg, and the Rust twin
# (`gate_eligible_receipt`) kept a hard-coded length pin, so `fno-agents
# verify-evidence receipt` went on rejecting a green preflight. Both readers
# looked correct in isolation. The failure is silent - the receipt is DISCARDED,
# not rejected loudly - so nothing surfaces until a `preflight.required = true`
# install can never clear its evidence gate. A comment saying "keep these in
# step" is what was there before, and it did not keep them in step.
#
# The four sites:
#   1-2. scripts/ci/preflight.sh, both REQUIRED_SCOPE_NAMES blocks (they are
#        themselves two copies of one list, and must agree with each other)
#   3.   cli/src/fno/pr/_preflight.py  _PREFLIGHT_BASE_SCOPE/_OPTIONAL_SCOPE
#   4.   crates/fno-agents/src/verify_evidence.rs  PREFLIGHT_BASE/OPTIONAL_SCOPE
set -uo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root" || exit 1

SH="scripts/ci/preflight.sh"
PY="cli/src/fno/pr/_preflight.py"
RS="crates/fno-agents/src/verify_evidence.rs"

fail() {
  echo "preflight-scope-parity: $*" >&2
  exit 1
}

for f in "$SH" "$PY" "$RS"; do
  [ -f "$f" ] || fail "missing declaration site $f"
done

# preflight.sh: the base assignment plus every `+=(name)` append, across BOTH
# blocks. Both blocks feed the same union deliberately - a leg that appears in
# only one of them is already a bug the union cannot see, and the two-copy
# problem is called out in the header rather than silently tolerated.
sh_names() {
  awk '
    /REQUIRED_SCOPE_NAMES=\(/ {
      line = $0
      sub(/.*REQUIRED_SCOPE_NAMES=\(/, "", line)
      sub(/\).*/, "", line)
      n = split(line, parts, /[ \t]+/)
      for (i = 1; i <= n; i++) if (parts[i] != "") print parts[i]
    }
    /REQUIRED_SCOPE_NAMES\+=\(/ {
      line = $0
      sub(/.*REQUIRED_SCOPE_NAMES\+=\(/, "", line)
      sub(/\).*/, "", line)
      if (line != "") print line
    }
  ' "$SH" | LC_ALL=C sort -u
}

# Quoted "name" tokens inside a named block, up to its closing delimiter. The
# `:` test (or the literal `smoke`) is what separates a leg name from the other
# strings that share those blocks.
block_names() {
  awk -v start="$2" -v end="$3" '
    index($0, start) { on = 1 }
    on {
      s = $0
      while (match(s, /"[A-Za-z0-9:_.-]+"/)) {
        tok = substr(s, RSTART + 1, RLENGTH - 2)
        if (tok ~ /:/ || tok == "smoke") print tok
        s = substr(s, RSTART + RLENGTH)
      }
      if (index($0, end)) on = 0
    }
  ' "$1" | LC_ALL=C sort -u
}

sh_all=$(sh_names)
py_base=$(block_names "$PY" "_PREFLIGHT_BASE_SCOPE = frozenset(" ")")
py_opt=$(block_names "$PY" "_PREFLIGHT_OPTIONAL_SCOPE = frozenset(" ")")
py_all=$(printf '%s\n%s\n' "$py_base" "$py_opt" | grep -v '^$' | LC_ALL=C sort -u)
# `];` rather than `]`: the declaration line itself carries `[&str; 5] = [`, so
# a bare `]` closes the block on the line it opened and reads zero names.
rs_base=$(block_names "$RS" "const PREFLIGHT_BASE_SCOPE" "];")
rs_opt=$(block_names "$RS" "const PREFLIGHT_OPTIONAL_SCOPE" "];")
rs_all=$(printf '%s\n%s\n' "$rs_base" "$rs_opt" | grep -v '^$' | LC_ALL=C sort -u)

# Fail on a zero read rather than letting two empty sets compare equal. An
# instrument that matched nothing would otherwise report perfect agreement,
# which is the absence-as-success shape this repo's pitfalls corpus refuses.
[ -n "$sh_all" ] || fail "read zero leg names from $SH; the instrument matched nothing"
[ -n "$py_all" ] || fail "read zero leg names from $PY; the instrument matched nothing"
[ -n "$rs_all" ] || fail "read zero leg names from $RS; the instrument matched nothing"

report_diff() {
  left_label="$1"
  right_label="$2"
  left="$3"
  right="$4"
  only_left=$(printf '%s\n' "$left" | grep -vxF -f <(printf '%s\n' "$right") | tr '\n' ' ')
  only_right=$(printf '%s\n' "$right" | grep -vxF -f <(printf '%s\n' "$left") | tr '\n' ' ')
  case "$only_left" in
    *[!\ ]*) echo "  only in $left_label: $only_left" >&2 ;;
  esac
  case "$only_right" in
    *[!\ ]*) echo "  only in $right_label: $only_right" >&2 ;;
  esac
}

if [ "$sh_all" != "$py_all" ]; then
  report_diff "$SH" "$PY" "$sh_all" "$py_all"
  fail "preflight.sh and the Python gate disagree about the receipt scope"
fi
if [ "$sh_all" != "$rs_all" ]; then
  report_diff "$SH" "$RS" "$sh_all" "$rs_all"
  fail "preflight.sh and the Rust gate disagree about the receipt scope"
fi

count=$(printf '%s\n' "$sh_all" | awk 'NF {n++} END {print n+0}')
echo "preflight-scope-parity: $count legs agree across preflight.sh, the Python gate and the Rust gate"

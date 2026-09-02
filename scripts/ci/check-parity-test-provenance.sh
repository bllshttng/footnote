#!/usr/bin/env bash
# scripts/ci/check-parity-test-provenance.sh
#
# Every `crates/*/tests/*_parity.rs` declares what it IS, and this gate asserts
# that declaration against the filesystem in BOTH directions.
#
#   //! parity-stage: differential | characterization
#   //! parity-oracle: <repo-relative path> | <python.dotted.leg>
#
# A `differential` file pins a LIVE second implementation, so its oracle must
# resolve. A `characterization` file is a finished port frozen against goldens,
# so its oracle must be GONE.
#
# What an oracle names: the LEG, not a file that happens to hold it. A
# repo-relative path names a whole-file leg (the bash case). A dotted name
# resolves to cli/src/<a/b/c>.py when that file exists; otherwise its last
# segment is a SYMBOL and the leg is that symbol inside the parent module
# (cli/src/<a/b>.py defining `def c`/`class c`/`c =`/`c:` at top level). The
# symbol form is how a leg inside a SURVIVING module is named: a port that
# deletes the ask functions flips the oracle to gone even though the module
# file remains, which is the case for every Python dual implementation. The
# file wins when it exists, so a module-level oracle keeps meaning the module.
#
# Why two-sided. A one-sided check ("the oracle exists") passes a finished port
# that still advertises a live second implementation, and a filename ending
# `_parity.rs` then reads as a dual implementation to anyone taking inventory.
# That is not hypothetical: two of this repo's four parity tests were counted
# as live dual implementations when both had been frozen against bash oracles
# that were already deleted. The declaration makes the difference machine
# readable, and the reverse assertion is what catches the next one.
#
# The header carries NO node id and NO PR number: scripts/ci/check-no-internal
# -refs.sh fails on them. The oracle is the identity.
#
# The pass is NOT silent. It prints one line per file naming the stage and the
# RESOLVED oracle, because a green with no positive marker is exactly the
# absence this gate exists to replace, and a dotted oracle resolved to the
# wrong path would otherwise pass unseen.
#
# Usage: check-parity-test-provenance.sh [--root <dir>]
#
# Exit codes:
#   0  every parity file declares a stage and an oracle that agrees with disk
#   1  a declaration is missing, malformed, or contradicted by the filesystem
#   2  bad invocation
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) ROOT="$2"; shift 2 ;;
    # Print the whole header, bounded by the first line of code rather than by
    # a line number. A hardcoded range silently truncates the moment the header
    # grows, and the first thing it cuts is the tail of the exit-code list.
    -h|--help) sed -n '/^set -uo/q;p' "$0"; exit 0 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ ! -d "$ROOT" ]]; then
  echo "ERROR: --root is not a directory: $ROOT" >&2
  exit 2
fi

fail=0
refuse() { echo "FAIL: $*" >&2; fail=1; }

# The required form, printed with every header refusal so the author never has
# to come read this file to learn it.
required_form() {
  {
    echo "  see docs/architecture/dual-implementation-inventory.md"
    echo "  required header form, above the existing prose:"
    echo "    //! parity-stage: differential | characterization"
    echo "    //! parity-oracle: <repo-relative path> | <python.dotted.module> | <python.dotted.module>.<symbol>"
  } >&2
}

# Read one `//! <field>: <value>` out of a file's header. Empty when absent.
read_field() { # <file> <field>
  sed -n "s|^//! *$2: *||p" "$1" | head -1 | sed 's/[[:space:]]*$//'
}

# Map a dotted oracle to its repo-relative MODULE path (the whole dotted name
# read as a module). A slash-bearing oracle is already a path.
oracle_module() { # <oracle>
  case "$1" in
    */*) printf '%s' "$1" ;;
    *) printf 'cli/src/%s.py' "$(printf '%s' "$1" | tr '.' '/')" ;;
  esac
}

# Map a dotted oracle to its PARENT module path (dotted name minus the last
# segment): the module a symbol-form leg is defined in.
oracle_parent() { # <oracle>
  printf 'cli/src/%s.py' "$(printf '%s' "${1%.*}" | tr '.' '/')"
}

# Whether the oracle identifies a leg still on disk. The module path itself
# resolves when it exists; otherwise (dotted names only) the last segment is
# a symbol, and it resolves when the parent module defines it at top level.
oracle_resolves() { # <oracle>
  local module
  module="$(oracle_module "$1")"
  if [[ -e "$ROOT/$module" ]]; then
    return 0
  fi
  case "$1" in
    */*) return 1 ;;
  esac
  local parent sym
  parent="$(oracle_parent "$1")"
  sym="${1##*.}"
  [[ -f "$ROOT/$parent" ]] || return 1
  grep -qE "^(async def|def|class)[[:space:]]+${sym}([(]|$)|^${sym}[[:space:]]*[:=]" "$ROOT/$parent" 2>/dev/null
}

shopt -s nullglob
files=("$ROOT"/crates/*/tests/*_parity.rs)
shopt -u nullglob

if [[ ${#files[@]} -eq 0 ]]; then
  # A zero-file pass is an absence, and an absence has three explanations: no
  # parity tests, a moved directory, or a broken glob. Refuse rather than
  # report a green this gate did not earn.
  echo "FAIL: no *_parity.rs found under $ROOT/crates/*/tests/" >&2
  exit 1
fi

for file in "${files[@]}"; do
  rel="${file#"$ROOT"/}"
  stage="$(read_field "$file" 'parity-stage')"
  oracle="$(read_field "$file" 'parity-oracle')"

  if [[ -z "$stage" || -z "$oracle" ]]; then
    refuse "$rel declares no parity provenance (stage='$stage' oracle='$oracle')."
    required_form
    continue
  fi

  if [[ "$stage" != "differential" && "$stage" != "characterization" ]]; then
    refuse "$rel declares parity-stage '$stage', which is not one of: differential, characterization."
    required_form
    continue
  fi

  resolved="$(oracle_module "$oracle")"
  if [[ "$oracle" != */* && ! -e "$ROOT/$resolved" ]]; then
    resolved="$(oracle_parent "$oracle"):${oracle##*.}"
  fi

  if [[ "$stage" == "differential" ]]; then
    if oracle_resolves "$oracle"; then
      echo "ok   $rel  stage=differential      oracle=$resolved (present)"
    else
      refuse "$rel is differential but its oracle is gone: $resolved"
      echo "  the port appears finished. Capture goldens from the old leg while" >&2
      echo "  it still exists, then convert this to a characterization test." >&2
      echo "  See docs/architecture/dual-implementation-inventory.md." >&2
    fi
  else
    if oracle_resolves "$oracle"; then
      refuse "$rel is characterization but its oracle is LIVE: $resolved"
      echo "  a characterization test stands in for a DELETED leg. This oracle" >&2
      echo "  still exists, so either the stage is wrong or the port is not done." >&2
      echo "  See docs/architecture/dual-implementation-inventory.md." >&2
    else
      echo "ok   $rel  stage=characterization  oracle=$resolved (absent, as required)"
    fi
  fi
done

if [[ $fail -ne 0 ]]; then
  exit 1
fi

echo "check-parity-test-provenance: ${#files[@]} parity file(s) declare a stage that agrees with disk"

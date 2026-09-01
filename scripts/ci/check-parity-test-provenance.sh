#!/usr/bin/env bash
# scripts/ci/check-parity-test-provenance.sh
#
# Every `crates/*/tests/*_parity.rs` declares what it IS, and this gate asserts
# that declaration against the filesystem in BOTH directions.
#
#   //! parity-stage: differential | characterization
#   //! parity-oracle: <repo-relative path> | <python.dotted.module>
#
# A `differential` file pins a LIVE second implementation, so its oracle must
# exist. A `characterization` file is a finished port frozen against goldens,
# so its oracle must be GONE.
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
# RESOLVED oracle path, because a green with no positive marker is exactly the
# absence this gate exists to replace, and a dotted module resolved to the
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
    -h|--help) sed -n '1,33p' "$0"; exit 0 ;;
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
    echo "  required header form, above the existing prose:"
    echo "    //! parity-stage: differential | characterization"
    echo "    //! parity-oracle: <repo-relative path> | <python.dotted.module>"
  } >&2
}

# Read one `//! <field>: <value>` out of a file's header. Empty when absent.
read_field() { # <file> <field>
  sed -n "s|^//! *$2: *||p" "$1" | head -1 | sed 's/[[:space:]]*$//'
}

# An oracle with a slash is a repo-relative path. Anything else is a Python
# dotted module, resolved under cli/src/ for the existence test.
resolve_oracle() { # <oracle>
  case "$1" in
    */*) printf '%s' "$1" ;;
    *) printf 'cli/src/%s.py' "$(printf '%s' "$1" | tr '.' '/')" ;;
  esac
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

  resolved="$(resolve_oracle "$oracle")"

  if [[ "$stage" == "differential" ]]; then
    if [[ -e "$ROOT/$resolved" ]]; then
      echo "ok   $rel  stage=differential      oracle=$resolved (present)"
    else
      refuse "$rel is differential but its oracle is gone: $resolved"
      echo "  the port appears finished. Capture goldens from the old leg while" >&2
      echo "  it still exists, then convert this to a characterization test." >&2
      echo "  See docs/architecture/dual-implementation-inventory.md." >&2
    fi
  else
    if [[ -e "$ROOT/$resolved" ]]; then
      refuse "$rel is characterization but its oracle is LIVE: $resolved"
      echo "  a characterization test stands in for a DELETED leg. This oracle" >&2
      echo "  still exists, so either the stage is wrong or the port is not done." >&2
    else
      echo "ok   $rel  stage=characterization  oracle=$resolved (absent, as required)"
    fi
  fi
done

if [[ $fail -ne 0 ]]; then
  exit 1
fi

echo "check-parity-test-provenance: ${#files[@]} parity file(s) declare a stage that agrees with disk"

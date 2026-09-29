#!/usr/bin/env bash
# Every install command README.md advertises must be proven by an expect: pass
# row of the install-channel matrix. Scans the fenced lines under the
# "## Install" heading, plus fenced "npx skills add" lines anywhere (the
# skills channel is advertised outside Install too). Strips "Label:" prefixes
# and trailing "# comment" text, then requires each normalized command to
# appear verbatim in some pass row's readme list in
# .github/workflows/install-channels.yml. Prints one line per unproven
# command; exits 1 when any exists, 0 when all are proven.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
README="$ROOT/README.md"
WORKFLOW="$ROOT/.github/workflows/install-channels.yml"

for f in "$README" "$WORKFLOW"; do
  [ -f "$f" ] || { echo "missing $f"; exit 1; }
done

# The readme strings of every expect: pass row, one per line. Field order in
# the matrix entries is stable: id, runs-on, expect, node, readme.
proven="$(awk '
  /^[[:space:]]*- id:/ { expect = "" }
  /^[[:space:]]*expect:/ { expect = $2 }
  expect == "pass" && /^[[:space:]]*readme:/ { print }
' "$WORKFLOW" | grep -oE "'[^']*'" | sed -e "s/^'//" -e "s/'\$//" || true)"

if [ -z "$proven" ]; then
  echo "no expect: pass row in $WORKFLOW carries readme strings; nothing can be proven"
  exit 1
fi

status=0
in_install=0
in_fence=0
lineno=0
while IFS= read -r line; do
  lineno=$((lineno + 1))
  if [ "$in_install" -eq 0 ]; then
    if [ "$line" = "## Install" ]; then
      in_install=1
    fi
  elif [ "${line%% *}" = "##" ] || [ "${line%% *}" = "#" ]; then
    in_install=0
  fi
  case "$line" in
    '```'*)
      if [ "$in_fence" -eq 1 ]; then in_fence=0; else in_fence=1; fi
      continue
      ;;
  esac
  [ "$in_fence" -eq 1 ] || continue
  if [ "$in_install" -eq 0 ]; then
    case "$line" in
      *"npx skills add"*) ;;
      *) continue ;;
    esac
  fi
  cmd="$(printf '%s\n' "$line" \
    | sed -e 's/^[[:space:]][[:space:]]*//' \
          -e 's/^[[:alnum:]][[:alnum:] ._-]*:[[:space:]][[:space:]]*//' \
          -e 's/[[:space:]][[:space:]]*#[^#]*$//' \
          -e 's/[[:space:]]*$//')"
  [ -n "$cmd" ] || continue
  if ! printf '%s\n' "$proven" | grep -qxF "$cmd"; then
    echo "README.md:$lineno: unproven install command: $cmd"
    status=1
  fi
done < "$README"

if [ "$status" -ne 0 ]; then
  echo "README advertises install commands no expect: pass matrix row proves"
  exit 1
fi
echo "every README install command is proven by an expect: pass matrix row"

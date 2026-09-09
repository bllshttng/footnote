#!/usr/bin/env bash
# Every event kind the reign arm list names must have a producer.
#
# Measured 2026-09-09: one event kind named in the arm list matched ZERO files
# under cli, crates, hooks and scripts, while the positive controls matched 6
# to 89 files with the same grep. It was prose fiction: every king arming that
# monitor watched for a signal nothing emits, and a wrong reader is
# indistinguishable from a quiet board - the exact failure the arm-list
# paragraph itself warns about. A documented event with no producer reads as a
# quiet board forever.
#
# The check: extract every backticked token from the numbered arm lines of
# skills/reign/SKILL.md and docs/architecture/reign.md - a bare word counts in
# full, so a one-word kind with no underscore is still checked - subtract the
# allowlist of words that are reader fields, graph fields, code paths, harness
# vocabulary or check-run vocabulary rather than journal events, and refuse
# any remaining token with zero matching files. The producer grep demands the
# token appear as a QUOTED literal (a JSON key value, a string argument) in
# cli, crates, hooks or scripts, and skips vendored, build and VCS trees; one
# matching file passes, because the bar is "named in the system's emit
# vocabulary", not "is hot". A bare mention in a comment or prose does not
# count: a kind named only in prose is exactly the fiction this refuses.
# docs/ is deliberately NOT scanned for the same reason.
#
# Targets default to the two files in the checkout this script runs from, so
# the arm list and its guard move together; a kind dropped from the arm list
# stops being checked the same commit, which is the honest direction to fail
# in. Optional args replace the defaults (the seeded-token negative test uses
# that).
#
# Exit 0 naming every token covered; exit 1 naming the file and token
# otherwise.
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

FILES=()
if [[ $# -gt 0 ]]; then
  FILES=("$@")
else
  FILES=("$REPO_ROOT/skills/reign/SKILL.md" "$REPO_ROOT/docs/architecture/reign.md")
fi

# Not journal events: fno doctor event find output fields; graph and feed
# fields; source paths; the reign registry reader and a capacity reading;
# check-run conclusions, verdict tokens and court words from the CI and
# liveness arms' rules.
ALLOWED='file_count
match_count
unreadable_files
total_count
pr_number
pr_created
_kanban_column
pr_watch
_king_wake
loop_reign
reign_state
sustained_cpu_cores
timed_out
action_required
startup_failure
red
green
pending
conclusion
failure
error
cancelled
completed
state
success
split
conflicts'

SCAN_DIRS=()
for d in cli crates hooks scripts; do
  if [[ -d "$REPO_ROOT/$d" ]]; then
    SCAN_DIRS+=("$REPO_ROOT/$d")
  fi
done
if [[ ${#SCAN_DIRS[@]} -eq 0 ]]; then
  echo "check-reign-arm-signals: no cli/crates/hooks/scripts under $REPO_ROOT to scan" >&2
  exit 1
fi

covered=""
fail=0
banner=0
for file in "${FILES[@]}"; do
  if [[ ! -f "$file" ]]; then
    echo "check-reign-arm-signals: $file not found" >&2
    exit 1
  fi
  # The arm list is the numbered "**Name, cadence.**" lines inside the arm
  # section; any other heading ends it. A span that is ONE bare word is a
  # candidate in full (one-word kinds have no underscore to find); any other
  # span (a command, a path, a tuple) gives up its snake_case tokens.
  tokens="$(awk '/Arm the beat|six arms and why each exists/{f=1;next} /^## /{f=0} f && /^[0-9]+\. \*\*/{print}' "$file" \
    | (grep -o '`[^`]*`' || true) | tr -d '`' \
    | while IFS= read -r span; do
        if [[ -z "$span" ]]; then
          continue
        elif printf '%s' "$span" | grep -qE '^_?[a-z][a-z0-9_]*$'; then
          printf '%s\n' "$span"
        else
          printf '%s\n' "$span" | (grep -oE '_?[a-z][a-z0-9]*(_[a-z0-9]+)+' || true)
        fi
      done \
    | sort -u)"
  while IFS= read -r tok; do
    [[ -z "$tok" ]] && continue
    if printf '%s\n' "$ALLOWED" | grep -qxF -- "$tok"; then
      continue
    fi
    case " $covered " in *" $tok "*) continue ;; esac
    tf="$(mktemp "${TMPDIR:-/tmp}/reign-arm-signals.XXXXXX")"
    grep -rlE --exclude-dir=.venv --exclude-dir=target --exclude-dir=.git --exclude-dir=node_modules -I -- "(\"|')${tok}(\"|')" "${SCAN_DIRS[@]}" > "$tf" 2>/dev/null || true
    if [[ -s "$tf" ]]; then
      covered="$covered $tok"
    else
      if [[ $banner -eq 0 ]]; then
        echo "check-reign-arm-signals: FAIL - a documented event has no producer." >&2
        echo "  These arm-list names match zero files under cli/crates/hooks/scripts:" >&2
        banner=1
      fi
      echo "  $file: $tok" >&2
      fail=1
    fi
    rm -f "$tf"
  done <<< "$tokens"
done

if [[ $fail -ne 0 ]]; then
  echo "" >&2
  echo "  A documented event with no producer reads as a quiet board forever: every" >&2
  echo "  king armed on that name watches for a signal nothing emits. Fix: name a" >&2
  echo "  kind that exists (check with: fno doctor event find <kind> --since 7d -J)," >&2
  echo "  or land the producer the documentation promises." >&2
  exit 1
fi

echo "check-reign-arm-signals: ok (every arm-list event name has an emit site; covered:$covered)"

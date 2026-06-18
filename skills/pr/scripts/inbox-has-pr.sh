#!/usr/bin/env bash
# Idempotency guard for /fno:pr merged.
#
# A post-merge inbox section is keyed by an HTML-comment marker that embeds the
# PR number: `<!-- post-merge:pr-<N> -->`. This script answers "has the ritual
# already written a section for this PR?" so a re-run (or a racing trigger) is a
# no-op instead of appending a duplicate.
#
# Usage:   inbox-has-pr.sh <inbox_path> <pr_number>
# Exit 0:  marker present  -> section already written, SKIP writing prose
# Exit 1:  marker absent (or inbox file does not exist yet) -> safe to write
# Exit 2:  usage error
#
# Pure file inspection; no `fno`/`gh` dependency, so it stays deterministically
# testable regardless of installed-CLI version.
set -euo pipefail

inbox_path="${1:-}"
pr="${2:-}"

if [[ -z "$inbox_path" || -z "$pr" ]]; then
  echo "usage: inbox-has-pr.sh <inbox_path> <pr_number>" >&2
  exit 2
fi

# Tolerate a leading '#': callers may pass "#123" or "123".
pr="${pr#\#}"

if ! [[ "$pr" =~ ^[0-9]+$ ]]; then
  echo "error: pr_number must be numeric, got: ${pr}" >&2
  exit 2
fi

marker="<!-- post-merge:pr-${pr} -->"

if [[ -f "$inbox_path" ]] && grep -qF "$marker" "$inbox_path"; then
  exit 0
fi
exit 1

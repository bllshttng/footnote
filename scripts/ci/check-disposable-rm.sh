#!/usr/bin/env bash
# check-disposable-rm.sh - refuse a bare `rm` in the files that delete
# fno-owned disposable state under a lock or at worktree scale.
#
# On a machine whose `rm` is wrapped to a trash tool (a common safety setup), a
# bare `rm` MOVES the path instead of unlinking it. Almost every `rm` in this
# repo tolerates that: the path leaves its old location either way, so the
# script's own logic is unaffected and the cost is disk on that host. Two files
# do not, and they are this gate's ALLOWLIST:
#
#   scripts/ci/preflight.sh
#     (a) concurrency-critical. Every `rm` there sits in, or tears down, the
#     mutex around the one shared preflight worktree. A delete that silently
#     becomes a move adds non-atomic, unbounded-latency work inside that
#     section, next to a rename-based steal protocol the comments there
#     document as already once-bitten.
#   hooks/worktree-remove.sh
#     (b) potentially large. A worktree with a built cargo target dir runs to
#     gigabytes and git already holds every byte. Trashing one relocates the
#     bytes instead of reclaiming them, which inverts the operation's purpose.
#
# Sanctioned spellings (the two-rung form the guarded files use):
#   command -p rm   resolves via the default PATH, so a wrapper earlier in the
#                   user's PATH cannot intercept it. Portable where /bin holds
#                   only sh (NixOS).
#   /bin/rm         absolute path. The fallback rung, for a shell whose default
#                   PATH is itself shadowed.
#   `command rm` is NOT sanctioned: it bypasses functions and aliases only,
#   not a PATH entry, so under a wrapper it still trashes.
#
# This is an allowlist, not a repo-wide ban. The other ~339 bare `rm` sites in
# hooks/, scripts/, tests/ and cli/ are correct as they are; the criterion and
# the probe table live in docs/architecture/disposable-deletes.md.
#
# Run: bash scripts/ci/check-disposable-rm.sh [file ...]
# No args: check the allowlist below. With args: check each named file as if
# allowlisted (used by the self-test). Exits 0 clean. Exits 1 naming
# file:line and the criterion. A listed file that is missing or unreadable
# fails closed: a vanished lock-protocol file is exactly the case where this
# guard must complain.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

ALLOWLIST=(
  "scripts/ci/preflight.sh|criterion (a) concurrency-critical: preflight lock protocol and teardown"
  "hooks/worktree-remove.sh|criterion (b) potentially large: worktree removal must unlink, not trash-move"
)

VIOLATIONS=0

report() {
  echo "check-disposable-rm: $1" >&2
}

check_file() {
  local path="$1" criterion="$2"
  if [[ ! -f "$path" ]]; then
    report "${path}: listed for checking but missing or unreadable - fail closed (${criterion})"
    VIOLATIONS=$((VIOLATIONS + 1))
    return
  fi
  # Detection, per line: strip double- then single-quoted strings (an `rm`
  # inside an echo message is advice to a human, not a call), strip the
  # comment (after string-stripping, the first # starts one), then mask the
  # sanctioned spellings. What survives is a command word `rm`.
  local hits
  hits="$(
    nl -ba "$path" \
      | sed -E 's/"[^"]*"//g; s/'"'"'[^'"'"']*'"'"'//g; s/#.*$//;
                s/command[[:space:]]+-p[[:space:]]+rm/RM_SANCTIONED/g; s#/bin/rm#RM_SANCTIONED#g' \
      | awk -F'\t' '$0 ~ /(^|[^A-Za-z0-9_\/.-])rm([^A-Za-z0-9_-]|$)/ {print $1}'
  )"
  [[ -z "$hits" ]] && return
  local n
  for n in $hits; do
    report "${path}:${n}: bare \`rm\` in a disposable-delete file (${criterion})."
  done
  report "  Use the two-rung form: command -p rm <args> 2>/dev/null || /bin/rm <args>."
  report "  Or call the file's srm() helper where one exists. \`command rm\` does not bypass a"
  report "  PATH wrapper. See docs/architecture/disposable-deletes.md."
  VIOLATIONS=$((VIOLATIONS + 1))
}

if [[ $# -gt 0 ]]; then
  for f in "$@"; do
    check_file "$f" "explicitly listed"
  done
else
  for entry in "${ALLOWLIST[@]}"; do
    check_file "$ROOT/${entry%%|*}" "${entry#*|}"
  done
fi

if (( VIOLATIONS > 0 )); then
  report "${VIOLATIONS} file(s) with bare \`rm\` in a disposable-delete path."
  exit 1
fi
echo "check-disposable-rm: no bare rm in the guarded disposable-delete files"
exit 0

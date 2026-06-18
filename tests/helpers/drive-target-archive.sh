#!/usr/bin/env bash
# drive-target-archive.sh — surgical driver for the artifact-archive code path.
#
# Usage:
#   bash tests/helpers/drive-target-archive.sh <state-file-path>
#
# Sets up the minimal env that scripts/lib/archive-artifacts.sh expects
# (REPO_ROOT, LOG_FILE) and invokes _archive_artifacts directly, isolating
# it from the session-cost, register-task, and completion-summary paths that
# require real transcripts.

set -uo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <state-file-path>" >&2
    exit 2
fi

STATE_FILE="$1"
if [[ ! -f "$STATE_FILE" ]]; then
    echo "drive-target-archive: state file not found: $STATE_FILE" >&2
    exit 2
fi

# Derive REPO_ROOT from the state file's .fno directory (parent-of-parent).
STATE_ABS=$(cd "$(dirname "$STATE_FILE")" && pwd)/$(basename "$STATE_FILE")
REPO_ROOT="$(cd "$(dirname "$STATE_ABS")/.." && pwd)"
export REPO_ROOT

LOG_FILE="${LOG_FILE:-${REPO_ROOT}/.fno/drive-target-archive.log}"
export LOG_FILE
mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null || true

# Resolve the archive-artifacts lib path. Prefer the project's own copy when
# this driver runs inside the abilities repo checkout; fall back to the
# installed plugin location otherwise.
ARCHIVE_LIB=""
SCRIPT_DIR_OF_TEST="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ -f "${SCRIPT_DIR_OF_TEST}/scripts/lib/archive-artifacts.sh" ]]; then
    ARCHIVE_LIB="${SCRIPT_DIR_OF_TEST}/scripts/lib/archive-artifacts.sh"
elif [[ -f "${FNO_LIB:-}/archive-artifacts.sh" ]]; then
    ARCHIVE_LIB="${FNO_LIB}/archive-artifacts.sh"
fi

if [[ -z "$ARCHIVE_LIB" || ! -f "$ARCHIVE_LIB" ]]; then
    echo "drive-target-archive: cannot locate archive-artifacts.sh" >&2
    exit 2
fi

# shellcheck source=/dev/null
source "$ARCHIVE_LIB"

_archive_artifacts "$STATE_FILE"

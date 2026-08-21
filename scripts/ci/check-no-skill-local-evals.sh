#!/usr/bin/env bash
# check-no-skill-local-evals.sh - CI gate: skills/<name>/evals/ is a dead
# convention. The sole home for an eval task is the repo-root bank
# (evals/bank/*.yaml), graded by `fno doctor evals`. A skills/<name>/evals/ directory
# has zero consumers - no runner reads it - so a file placed there looks
# adopted but runs never, which is how this node once concluded the eval system
# had "one user" when it had none. Deleting the last instance without a gate
# lets the next person recreate it; this gate makes the deletion durable.
#
# Run: bash scripts/ci/check-no-skill-local-evals.sh
# Exits 0 when no skills/*/evals/ exists; exits 1 naming the offenders.
set -euo pipefail

REPO_ROOT=""
if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    REPO_ROOT="$git_root"
fi
if [[ -z "$REPO_ROOT" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
fi
cd "$REPO_ROOT"

# Capture the offender list via a checked command so a git failure is a loud
# error, not a vacuous clean pass (the process-substitution form hides git's
# exit status from `set -e`).
if ! offenders=$(git ls-files -- 'skills/*/evals/*'); then
    echo "check-no-skill-local-evals: 'git ls-files' failed (not a git repo or git unavailable)" >&2
    exit 1
fi

if [[ -z "$offenders" ]]; then
    echo "check-no-skill-local-evals: no skill-local evals directories (bank is the sole location)"
    exit 0
fi

{
    echo "check-no-skill-local-evals: skills/<name>/evals/ is a dead convention"
    echo "with zero consumers; found tracked files under it:"
    echo
    printf '%s\n' "$offenders"
    echo
    echo "Eval tasks live in evals/bank/*.yaml (graded by 'fno doctor evals'). Move the"
    echo "task there - the bank accepts arbitrary grade commands, so a skill-local"
    echo "eval has nowhere to go that a bank task cannot - and delete the directory."
    echo "A skills/<name>/evals file that runs never is worse than absent: it reads"
    echo "as an adopted convention and costs the next reader a wrong diagnosis."
} >&2
exit 1

#!/usr/bin/env bash
# check-no-internal-refs.sh - CI gate that blocks "internal/" vault-path
# literals from landing in shipped, user-facing content.
#
# "internal/" is the symlink to the maintainers' Obsidian vault. It exists
# only when Obsidian is enabled (config.obsidian.enabled); for an OSS install
# the path does not resolve. So a bare "internal/..." reference in user-facing
# content is either a dead pointer to a non-shipped vault file, or a path
# presented as universal that is actually vault-gated. Neither belongs in
# shipped content an OSS reader consumes.
#
# Run: bash scripts/ci/check-no-internal-refs.sh
# Exits 0 when clean; exits 1 with a report when violations are detected.
#
# Scope (what is scanned)
# -----------------------
#   docs/, agents/, commands/, and the top-level AGENTS.md / README.md /
#   CLAUDE.md / GEMINI.md - the user-facing prose surfaces.
#
# Out of scope (NOT scanned)
# --------------------------
#   The code tree + maintainer infra: cli/, crates/, scripts/, hooks/,
#   skills/, tests/, .claude/. There the Obsidian-gated resolver (canonical
#   example: cli/src/fno/paths.py), the worktree symlink infra
#   (scripts/setup/setup-worktree.sh, scripts/setup/worktree-create-hook.sh,
#   .gitignore), test fixtures (cli/tests/), and design-doc breadcrumbs in
#   docstrings legitimately reference "internal/". Those references are
#   correct (gated) and are deliberately left alone.
#
# Allowlist (scanned-but-exempt)
# ------------------------------
#   A handful of docs legitimately DOCUMENT the Obsidian-gated vault layout -
#   the "internal/" token is the documented subject (post-merge inbox_path,
#   cross-project inbox layout, triage default, reading-shipped-plans, the
#   megatron plan-write location, the _VAULT_TOPLEVEL_DIRS snippet), not a
#   leak. Add a doc here only when "internal/" is genuinely what it documents.

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

# Docs that legitimately document the Obsidian-gated vault layout.
ALLOWLIST=(
    "docs/architecture/auto-post-merge-ritual.md"
    "docs/architecture/cross-project-inbox.md"
    "docs/architecture/megatron.md"
    "docs/guides/cross-project-inbox.md"
    "docs/guides/reading-shipped-plans.md"
    "docs/system-architecture.md"
    "docs/triage.md"
)

is_allowlisted() {
    local f="$1" a
    for a in "${ALLOWLIST[@]}"; do
        [[ "$f" == "$a" ]] && return 0
    done
    return 1
}

VIOLATIONS=0
REPORT=""

# Capture the file list first so a git failure (not a repo, git unavailable) is
# a loud error, not a vacuous "no violations" pass: the `done < <(git ...)`
# process-substitution form hides git's exit status from `set -e`. (review #503, gemini)
if ! files_to_check=$(git ls-files -- 'docs/' 'agents/' 'commands/' 'AGENTS.md' 'README.md' 'CLAUDE.md' 'GEMINI.md'); then
    echo "check-no-internal-refs: 'git ls-files' failed (not a git repo or git unavailable)" >&2
    exit 1
fi

while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    is_allowlisted "$f" && continue
    hits=$(grep -n 'internal/' "$f" 2>/dev/null || true)
    [[ -z "$hits" ]] && continue
    while IFS= read -r line; do
        REPORT+="$f:$line"$'\n'
        VIOLATIONS=$((VIOLATIONS + 1))
    done <<< "$hits"
done <<< "$files_to_check"

if [[ $VIOLATIONS -eq 0 ]]; then
    echo "check-no-internal-refs: no violations found"
    exit 0
fi

{
    echo "check-no-internal-refs: $VIOLATIONS violation(s) found"
    echo
    printf '%s' "$REPORT"
    echo
    echo "internal/ is the maintainers' Obsidian-vault symlink and does not"
    echo "exist in an OSS install. In shipped user-facing content:"
    echo "  - dead design-doc / plan pointers: delete the pointer (keep node IDs)"
    echo "  - genuine vault-feature docs: reframe to drop the literal, or add the"
    echo "    file to ALLOWLIST in scripts/ci/check-no-internal-refs.sh"
    echo
    echo "The code tree (cli/, crates/, scripts/, hooks/, skills/) is not"
    echo "scanned; the Obsidian-gated resolver lives in cli/src/fno/paths.py."
} >&2
exit 1

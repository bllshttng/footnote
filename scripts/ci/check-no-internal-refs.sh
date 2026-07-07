#!/usr/bin/env bash
# check-no-internal-refs.sh - CI gate that blocks three leak classes from
# landing in shipped, user-facing prose:
#   internal-path  "internal/" vault-path literals (file-allowlisted below)
#   node-id        internal backlog node IDs (x-XXXX / ab-XXXXXXXX) - the
#                  standing "no internal node IDs in public docs" rule;
#                  exempt by TOKEN (documented format examples), never by file
#   session-url    a claude.ai/code session URL pasted into prose
# All three share the same scanned scope and the same fail-loud file-list
# capture. The commit-message / PR-body session-URL gate is a SEPARATE
# workflow (scripts/ci/check-no-session-urls.sh); this script covers prose.
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
#   _VAULT_TOPLEVEL_DIRS snippet), not a
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
    "docs/guides/cross-project-inbox.md"
    "docs/guides/reading-shipped-plans.md"
    "docs/path-config.md"
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

# node-id + session-url leak patterns (applied to ALL in-scope files).
NODE_ID_RE='\b(x-[0-9a-f]{4}|ab-[0-9a-f]{8})\b'
SESSION_URL_RE='claude\.ai/code'

# Synthetic example tokens that legitimately appear in format / command
# examples. Literal tokens only, all obviously non-real; exempt by TOKEN so a
# doc that shows an example ID never gains a blanket pass for a real leak
# (per the node-id scan's by-token, not by-file, contract).
NODE_ID_ALLOWLIST=(
    "ab-1a2b3c4d"   # slug-derivation example (ab-1a2b3c4d -> dashless-spawn)
    "ab-1234abcd"   # generic command / argument placeholder
)

# Echo the line with every allowlisted token removed. A line carrying ONLY
# example tokens then no longer matches NODE_ID_RE (not a violation); a real ID
# on the same line still trips.
strip_node_allowlist() {
    local line="$1" tok
    for tok in "${NODE_ID_ALLOWLIST[@]}"; do
        line="${line//$tok/}"
    done
    printf '%s' "$line"
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

    # internal-path scan: file-allowlisted (a handful of docs document the
    # vault layout). Byte-for-byte the original behavior.
    if ! is_allowlisted "$f"; then
        hits=$(grep -n 'internal/' "$f" 2>/dev/null || true)
        if [[ -n "$hits" ]]; then
            while IFS= read -r line; do
                REPORT+="[internal-path] $f:$line"$'\n'
                VIOLATIONS=$((VIOLATIONS + 1))
            done <<< "$hits"
        fi
    fi

    # node-id scan: token-allowlisted, applies to EVERY in-scope file (the
    # internal-path file-allowlist does not exempt a doc from the node-id rule).
    node_hits=$(grep -nE "$NODE_ID_RE" "$f" 2>/dev/null || true)
    if [[ -n "$node_hits" ]]; then
        while IFS= read -r line; do
            [[ -z "$line" ]] && continue
            if grep -qE "$NODE_ID_RE" <<< "$(strip_node_allowlist "$line")"; then
                REPORT+="[node-id] $f:$line"$'\n'
                VIOLATIONS=$((VIOLATIONS + 1))
            fi
        done <<< "$node_hits"
    fi

    # session-url scan: no exemptions, applies to every in-scope file.
    url_hits=$(grep -nE "$SESSION_URL_RE" "$f" 2>/dev/null || true)
    if [[ -n "$url_hits" ]]; then
        while IFS= read -r line; do
            [[ -z "$line" ]] && continue
            REPORT+="[session-url] $f:$line"$'\n'
            VIOLATIONS=$((VIOLATIONS + 1))
        done <<< "$url_hits"
    fi
done <<< "$files_to_check"

if [[ $VIOLATIONS -eq 0 ]]; then
    echo "check-no-internal-refs: no violations found"
    exit 0
fi

{
    echo "check-no-internal-refs: $VIOLATIONS violation(s) found"
    echo "(each line is prefixed with the leak class that matched)"
    echo
    printf '%s' "$REPORT"
    echo
    echo "[internal-path] internal/ is the maintainers' Obsidian-vault symlink"
    echo "  and does not exist in an OSS install:"
    echo "  - dead design-doc / plan pointers: delete the pointer"
    echo "  - genuine vault-feature docs: reframe to drop the literal, or add the"
    echo "    file to ALLOWLIST in scripts/ci/check-no-internal-refs.sh"
    echo
    echo "[node-id] internal backlog node IDs (x-XXXX / ab-XXXXXXXX) must not"
    echo "  appear in shipped prose (standing rule). Reword to describe the"
    echo "  feature or say 'a dedicated node' instead of naming the ID; drop the"
    echo "  parenthetical entirely where it was only a breadcrumb. A genuine"
    echo "  format/command EXAMPLE uses a synthetic token from NODE_ID_ALLOWLIST"
    echo "  (add one there only if it is obviously non-real)."
    echo
    echo "[session-url] a claude.ai/code session URL is an internal, irreversible"
    echo "  leak - remove it from the prose."
    echo
    echo "The code tree (cli/, crates/, scripts/, hooks/, skills/) is not"
    echo "scanned; the Obsidian-gated resolver lives in cli/src/fno/paths.py."
} >&2
exit 1

#!/usr/bin/env bash
# check-no-internal-refs.sh - CI gate that blocks four leak classes from
# landing in this repo:
#   internal-path    "internal/" vault-path literals (file-allowlisted below)
#   node-id          internal backlog node IDs (x-XXXX / ab-XXXXXXXX) - the
#                    standing "no internal node IDs in public docs" rule;
#                    exempt by TOKEN (documented format examples), never by file
#   session-url      a claude.ai/code session URL pasted into prose
#   competitor-name  a rival product's name anywhere in the tree (repo-wide,
#                    code included). The term list is base64-encoded below so
#                    this gate does not itself publish the names it guards.
# All three prose classes share the same scanned scope and the same fail-loud
# file-list capture. The competitor-name class scans every tracked file
# instead; see its block below. The commit-message / PR-body session-URL gate
# is a SEPARATE workflow (scripts/ci/check-no-session-urls.sh); this script
# covers checked-in content.
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

# node-id + session-url leak patterns (applied to ALL in-scope files). The
# session-url pattern requires an actual path char after /code/ (a real URL is
# claude.ai/code/<session-token>), so a doc that merely NAMES the concept - a
# bare claude.ai/code or an angle-bracket placeholder, including the docs for
# this very gate - is not a false positive.
NODE_ID_RE='\b(x-[0-9a-f]{4}|ab-[0-9a-f]{8})\b'
SESSION_URL_RE='claude\.ai/code/[A-Za-z0-9]'

# Synthetic example tokens that legitimately appear in format / command
# examples. Literal tokens only, all obviously non-real; exempt by TOKEN so a
# doc that shows an example ID never gains a blanket pass for a real leak
# (per the node-id scan's by-token, not by-file, contract).
NODE_ID_ALLOWLIST=(
    "ab-1a2b3c4d"   # slug-derivation example (ab-1a2b3c4d -> dashless-spawn)
    "ab-1234abcd"   # generic command / argument placeholder
    # Repeated-letter placeholders for examples needing several distinct nodes
    # (a symmetric edge takes two, a precedence chain three). Non-real by
    # construction: a minted id is random hex, never one letter repeated.
    "x-aaaa"
    "x-bbbb"
    "x-cccc"
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

# competitor-name scan: a rival product's name must not ship anywhere in this
# repo, code included, so this class walks every tracked file rather than the
# prose scope above. The term list is one base64 blob, one term per line when
# decoded, because a guard that named its terms would itself be the leak it
# exists to prevent. To maintain the list: write the terms one per line, then
#   <terms one per line> | base64 | tr -d '\n'
# and paste the single-line result below. Matching is case-insensitive and
# whole-word (git grep -w; POSIX ERE has no \b), so a Capitalized
# reintroduction fails and ordinary words that merely contain a term as a
# substring do not.
COMPETITOR_TERMS_B64='aGVyZHIKY211eApvcmNhCnN1cGVycG93ZXJzCmdldC1zaGl0LWRvbmUKZXZlcnl0aGluZy1jbGF1ZGUtY29kZQplY2MKY2N1c2FnZQphaWRlcgpyZXBvZ3JhbQphdXRvcmVzZWFyY2gKOXJvdXRlcgpjbGF1ZGUtbWVtCmNjLXN3aXRjaAphZ2VudHdvcmtmb3JjZQpjb2RleGJhcgpsaW5lYXJpcwptb3RpYQpjbGF1ZGUtY29kZS1yb3V0ZXIKcmFscGgKY2l0YWRlbAphbnRpZ3Jhdml0eS1mb3ItY2xhdWRlLWNvZGUK'

# Files exempt from the competitor-name scan. Each entry carries a WHY, so the
# next reader can audit the exemption instead of trusting it. Empty at
# landing; add a file only when the name in it is load-bearing and cannot be
# rewritten (a vendored license notice, for example).
NAME_ALLOWLIST=(
)

competitor_terms=$(printf '%s' "$COMPETITOR_TERMS_B64" | base64 -d 2>/dev/null)
if [[ -z "$competitor_terms" ]]; then
    echo "check-no-internal-refs: failed to decode the competitor term list" >&2
    exit 1
fi

while IFS= read -r term; do
    [[ -z "$term" ]] && continue
    if ! term_hits=$(git grep -Iinw -e "$term" 2>/dev/null); then
        continue
    fi
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        hit_file="${hit%%:*}"
        skip=0
        # ${arr[@]+"${arr[@]}"}: a plain "${arr[@]}" on an empty array aborts
        # under set -u on stock macOS bash 3.2.
        for exempt in ${NAME_ALLOWLIST[@]+"${NAME_ALLOWLIST[@]}"}; do
            [[ "$hit_file" == "$exempt" ]] && skip=1 && break
        done
        [[ $skip -eq 1 ]] && continue
        REPORT+="[competitor-name] $hit"$'\n'
        VIOLATIONS=$((VIOLATIONS + 1))
    done <<< "$term_hits"
done <<< "$competitor_terms"

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
    echo "[competitor-name] a rival product's name must not ship in this repo."
    echo "  Rewrite to describe the behavior, not its source: 'the lowercase"
    echo "  wire label' needs no provenance to be correct. If a hit is fno's"
    echo "  own vocabulary that merely collides with a term, add the file to"
    echo "  NAME_ALLOWLIST in scripts/ci/check-no-internal-refs.sh with a WHY"
    echo "  comment."
    echo
    echo "The code tree (cli/, crates/, scripts/, hooks/, skills/) is not"
    echo "scanned; the Obsidian-gated resolver lives in cli/src/fno/paths.py."
} >&2
exit 1

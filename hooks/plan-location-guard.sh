#!/usr/bin/env bash
# plan-location-guard.sh - PreToolUse hook: a NEW plan/design doc must be saved
# under the configured plans dir (x-5349, unified-dispatch PRD G10).
#
# `config.plans_dir` became settable (plus `fno do plan path`) without ever being
# enforced, so a plan still lands in whatever directory the agent picked. This
# is the positive half of that gate; the negative half - the write guards
# denying the CORRECT save through the internal/ symlink - is the carve-out in
# worktree-write-protect.sh. Both resolve the directory through
# helpers/plans-dir.sh so the two halves cannot disagree.
#
# Deliberately scoped to CREATION, not mutation:
#   - Write with plan-shaped frontmatter, or any add landing on a plans-glob
#     path, is a location decision and is gated.
#   - Edit is not. An Edit needs the file to already exist, so its location is
#     history; refusing it would only block repairs to legacy plans.
#
# Two block conditions, both requiring the target to be OUTSIDE the resolved
# plans dir:
#   A. Write of a *.md whose LEADING frontmatter block is plan-shaped
#      (`node:` + `slug:` + one of type/deliverable_type/status). Example
#      frontmatter quoted inside a doc body never matches - only the leading
#      `---` block is read.
#   B. Any created path matching */plans/*.md (the plans-glob half), which
#      catches a plan whose frontmatter has not been written yet.
#
# Fails OPEN throughout. An unresolvable plans dir (no `fno`, config error)
# means the convention cannot be stated, and a guard that cannot name the right
# destination must not block the wrong one.
#
# Known ceiling: this reads the write TOOLS agents create plans with (Write, and
# apply_patch adds). A plan written by shelling a heredoc (`cat > x.md <<EOF`)
# is not parsed and is not caught; extending it there means the shell-clause
# grammar graph-write-protect.sh carries, and no observed plan write takes that
# route. `fno do plan reconcile-status` remains the backstop for a drifted save.
#
# Exit 0 always; the decision travels in the stdout JSON.

set -uo pipefail

_HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=helpers/plans-dir.sh
source "${_HOOK_DIR}/helpers/plans-dir.sh" 2>/dev/null || { printf '%s\n' '{}'; exit 0; }
# Sourcing a TRUNCATED helper still exits 0 - bash runs it as it parses - so
# presence of the file is not presence of its functions. Without this the
# guard would block a correct save (a missing containment test reads as "not
# under the plans dir"), which contradicts the fail-open contract above. The
# sibling guard carries the same check for the same reason.
declare -F fno_plans_dir fno_physical_path fno_under_plans_dir >/dev/null 2>&1 \
    || { printf '%s\n' '{}'; exit 0; }

_approve() {
    printf '%s\n' '{}'
    exit 0
}

_block() {
    local reason="$1"
    if command -v jq >/dev/null 2>&1; then
        jq -nc --arg r "$reason" '{
            decision: "block",
            reason: $r,
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "deny",
                permissionDecisionReason: $r
            }
        }'
    else
        python3 -c 'import json,sys; r=sys.argv[1]; print(json.dumps({"decision":"block","reason":r,"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":r}}))' "$reason" 2>/dev/null \
            || printf '{"decision":"block","reason":"%s"}\n' "$reason"
    fi
    exit 0
}

PAYLOAD="$(cat)"

# ── 1. Pre-filter ─────────────────────────────────────────────────────────────
# Nothing here can fire without a `.md` target somewhere in the payload. Skip
# the parse (and the `fno` call behind it) for every other write.
[[ "$PAYLOAD" == *".md"* ]] || _approve

# ── 2. Parse (jq -> python3 -> approve) ───────────────────────────────────────
# Five newline-separated fields; `content` and `command` are flattened to one
# line each with \x1f standing in for the newlines we still need to inspect
# (tab and space are IFS-whitespace and would collapse empty fields).
TOOL="" FILE_PATH="" PAYLOAD_CWD="" CONTENT="" COMMAND=""
if command -v jq >/dev/null 2>&1; then
    { read -r TOOL; read -r FILE_PATH; read -r PAYLOAD_CWD; read -r CONTENT; read -r COMMAND; } < <(
        printf '%s' "$PAYLOAD" | jq -r '
            .tool_name // "",
            (.tool_input.file_path // ""),
            (.cwd // ""),
            (.tool_input.content // "" | gsub("\n";"")),
            (.tool_input.command // "" | gsub("\n";""))' 2>/dev/null
    )
elif command -v python3 >/dev/null 2>&1; then
    { read -r TOOL; read -r FILE_PATH; read -r PAYLOAD_CWD; read -r CONTENT; read -r COMMAND; } < <(
        printf '%s' "$PAYLOAD" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin); ti = d.get("tool_input") or {}
    print(d.get("tool_name") or "")
    print(ti.get("file_path") or "")
    print(d.get("cwd") or "")
    print((ti.get("content") or "").replace("\n", "\x1f"))
    print((ti.get("command") or "").replace("\n", "\x1f"))
except Exception:
    pass' 2>/dev/null
    )
else
    _approve
fi

# ── 3. Collect created targets ────────────────────────────────────────────────
# Write contributes its file_path; an apply_patch command contributes each
# `*** Add File:` path (a create). Updates and deletes are not location
# decisions and are skipped.
# Anchor relative targets to the SESSION cwd from the payload, not the cwd the
# harness happened to spawn this hook in - they differ for a worktree or
# multi-repo dispatch, and the containment test would then be decided against
# the wrong tree in silence. `pwd` only as a fallback.
CWD="$PAYLOAD_CWD"
[[ -n "$CWD" && -d "$CWD" ]] || CWD="$(pwd)"
TARGETS=()
if [[ "$TOOL" == "Write" && -n "$FILE_PATH" ]]; then
    TARGETS+=("$FILE_PATH")
fi
if [[ "$COMMAND" == *"*** Add File: "* ]]; then
    while IFS= read -r line; do
        case "$line" in
            "*** Add File: "*)
                p="${line#*: }"
                p="${p%$'\r'}"
                [[ -n "$p" ]] && TARGETS+=("$p")
                ;;
        esac
    done <<< "${COMMAND//$'\x1f'/$'\n'}"
fi
[[ ${#TARGETS[@]} -gt 0 ]] || _approve

# ── 4. Plan-shape reads ───────────────────────────────────────────────────────

# _is_test_path PATH -> 0 for scaffolding that legitimately holds plan-shaped
# docs outside the plans dir (fixtures, test trees). Mirrors the carve-out in
# graph-write-protect.sh so the two guards agree on what "not real" means.
_is_test_path() {
    case "$1" in
        */test/*|*/tests/*|*/fixtures/*|*/testdata/*) return 0 ;;
    esac
    return 1
}

# _has_plan_frontmatter -> 0 when $CONTENT opens with a `---` block carrying
# `node:` and `slug:` and at least one of type/deliverable_type/status. Only the
# LEADING block is read, so a doc quoting example frontmatter in its body is not
# a plan.
_has_plan_frontmatter() {
    local body="${CONTENT//$'\x1f'/$'\n'}" fm="" line seen_open=0
    [[ -n "$body" ]] || return 1
    while IFS= read -r line; do
        line="${line%$'\r'}"
        if [[ $seen_open -eq 0 ]]; then
            [[ "$line" == "---" ]] || return 1
            seen_open=1
            continue
        fi
        [[ "$line" == "---" || "$line" == "..." ]] && break
        fm+="$line"$'\n'
    done <<< "$body"
    [[ $seen_open -eq 1 ]] || return 1
    [[ "$fm" == *$'\n'"node:"* || "$fm" == "node:"* ]] || return 1
    [[ "$fm" == *$'\n'"slug:"* || "$fm" == "slug:"* ]] || return 1
    case "$fm" in
        *"type:"*|*"status:"*) return 0 ;;
    esac
    return 1
}

# ── 5. Verdict ────────────────────────────────────────────────────────────────
# Resolve from the SESSION cwd, exactly as the sibling carve-out does.
# `fno do plan path` is repo-anchored, so resolving from whatever cwd the
# harness spawned this hook in names a DIFFERENT project's plans dir on a
# worktree or multi-repo dispatch - which would deny a correct save while
# naming the wrong directory as the right one. Sharing a resolver is not
# enough; the two halves have to share the anchor too.
PLANS_DIR="$(cd "$CWD" 2>/dev/null && fno_plans_dir 2>/dev/null || true)"
[[ -n "$PLANS_DIR" ]] || _approve

for t in "${TARGETS[@]}"; do
    [[ "$t" == *.md ]] || continue
    _is_test_path "$t" && continue

    abs="$t"
    [[ "$abs" == /* ]] || abs="$CWD/$abs"
    fno_under_plans_dir "$PLANS_DIR" "$abs" && continue

    reason=""
    # B: a plans-glob path outside the configured dir. Checked first because it
    # holds even for a plan whose frontmatter is not written yet.
    case "$t" in
        */plans/*)
            reason="plan-location: \`$t\` is a plans path outside the configured plans dir ($PLANS_DIR)." ;;
    esac
    # A: plan-shaped frontmatter. Only a Write carries content to read.
    if [[ -z "$reason" && "$TOOL" == "Write" && "$t" == "$FILE_PATH" ]] && _has_plan_frontmatter; then
        reason="plan-location: \`$t\` has plan frontmatter (node/slug) but is outside the configured plans dir ($PLANS_DIR)."
    fi
    [[ -n "$reason" ]] || continue

    _block "$reason Resolve the save path with \`fno do plan path --slug \"<slug>\" [--node \"<node-id>\"]\` and write there - it joins the configured plans dir with the plans_filename template. Change the destination with \`fno config\` (plans_dir) or the plansDirectory setting, not by writing elsewhere."
done

_approve

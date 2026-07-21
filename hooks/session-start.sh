#!/usr/bin/env bash
# SessionStart hook for abilities plugin — cross-platform
# Injects project vision into session context.
# Wraps existing Claude Code-specific hooks and re-formats output per platform.

set -euo pipefail

# Check for jq dependency — exit silently if missing (enhancement hook)
if ! command -v jq &> /dev/null; then
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
STATE_FILE=".fno/target-state.md"

# ── Plugin-root pointer (best-effort, idempotent) ────────────────────
# Persist the plugin root to ~/.fno/plugin-root so `fno target init` and
# `fno gate set` can find their plugin scripts when run from a foreign project
# with no env hint. `fno` is a uv-tool install whose wheel does not carry
# hooks/, and CLAUDE_PLUGIN_ROOT is not propagated to arbitrary `fno`
# subprocesses - the pointer is then the only env-less source. PLUGIN_ROOT here
# is always the real plugin (parent of this hook's dir); the manifest check is
# a belt-and-suspenders guard. Mirrors fno.paths._persist_plugin_root and
# is read by fno.paths._read_persisted_plugin_root. errexit-safe.
prime_plugin_root_pointer() {
    [[ -f "$PLUGIN_ROOT/.claude-plugin/plugin.json" ]] || return 0
    local home="${FNO_HOME:-$HOME/.fno}"
    local ptr="$home/plugin-root"
    if [[ -f "$ptr" ]] && [[ "$(cat "$ptr" 2>/dev/null)" == "$PLUGIN_ROOT" ]]; then
        return 0
    fi
    mkdir -p "$home" 2>/dev/null || return 0
    printf '%s\n' "$PLUGIN_ROOT" > "$ptr" 2>/dev/null || true
}
prime_plugin_root_pointer || true

# ── .fno gitignore housekeeping (best-effort, idempotent) ────────────
# Keep machine-specific .fno/ session state (incl. target-state.md) out of the
# project repo so personal state never leaks into an open-source checkout. The
# helper no-ops when .fno/ is already ignored / absent / outside a git tree.
ensure_fno_gitignored() {
    local helper="$PLUGIN_ROOT/hooks/helpers/ensure-fno-gitignored.sh"
    [[ -f "$helper" ]] && bash "$helper" 2>/dev/null || true
}
ensure_fno_gitignored || true

# ── heal a wired Claude WorktreeRemove hook after a plugin move ──────
# That hook lives in ~/.claude/settings.json (claude rm runs with no session,
# so it never loads plugin hooks) and holds an ABSOLUTE script path. A plugin
# upgrade moves the versioned install dir out from under it and every
# hook-created worktree strands again, semi-silently. Gate on the plugin root
# actually changing, so the common session pays one string compare and no
# subprocess. Repair-only: never wires the hook for someone who did not ask.
heal_claude_worktree_hook() {
    local settings="$HOME/.claude/settings.json"
    [[ -f "$settings" ]] || return 0
    grep -q 'worktree-remove\.sh' "$settings" 2>/dev/null || return 0
    local home="${FNO_HOME:-$HOME/.fno}" stamp
    stamp="$home/.worktree-hook-root"
    [[ -f "$stamp" && "$(cat "$stamp" 2>/dev/null)" == "$PLUGIN_ROOT" ]] && return 0
    command -v fno >/dev/null 2>&1 || return 0
    # Stamp only on success. Stamping a failed repair (fno missing, or too old
    # to know --repair-only) would mark the heal done and never retry it.
    fno setup cli-hooks --no-codex --no-gemini --claude --repair-only \
        >/dev/null 2>&1 || return 0
    mkdir -p "$home" 2>/dev/null || return 0
    printf '%s\n' "$PLUGIN_ROOT" > "$stamp" 2>/dev/null || true
}
heal_claude_worktree_hook || true

# ── Platform detection ─────────────────────────────────────────────────
detect_platform() {
    # Explicit override wins. The Codex/Gemini hook installers set FNO_PLATFORM
    # in the command they write, because those CLIs do NOT set their plugin-root
    # env var when running a user-config hook - without this the hook would fall
    # through to `generic` and emit the legacy output shape instead of the
    # unified hookSpecificOutput.additionalContext (PR #11 codex review).
    case "${FNO_PLATFORM:-}" in
        claude|gemini|codex|cursor) echo "${FNO_PLATFORM}"; return ;;
    esac
    if [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
        echo "gemini"
    elif [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
        echo "codex"
    elif [[ -n "${CURSOR_PLUGIN_ROOT:-}" ]]; then
        echo "cursor"
    elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
        echo "claude"
    else
        echo "generic"
    fi
}

PLATFORM=$(detect_platform)

# Claude registers through its dedicated SessionStart hook in hooks.json.
# Codex uses this shared wrapper as its sole SessionStart entry, so register the
# addressable thread here exactly once. Supplying CODEX_PLUGIN_ROOT locally also
# covers the user-config fallback, which sets only FNO_PLATFORM=codex.
if [[ "$PLATFORM" == "codex" && -f "${SCRIPT_DIR}/register-session-start.sh" ]]; then
    CODEX_PLUGIN_ROOT="${CODEX_PLUGIN_ROOT:-$PLUGIN_ROOT}" \
        bash "${SCRIPT_DIR}/register-session-start.sh" || true
fi

hydrate_state_provider_context() {
    [[ -f "$STATE_FILE" ]] || return 0

    # Only hydrate state files owned by a live target run. Stale state from a
    # prior session must not be mutated — the stop hook will archive it.
    local guard_lib="${PLUGIN_ROOT}/scripts/lib/target-guard.sh"
    if [[ -f "$guard_lib" ]]; then
        # shellcheck source=../scripts/lib/target-guard.sh
        source "$guard_lib"
        target_is_active "$STATE_FILE" || return 0
    fi

    local timestamp
    timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    local temp_file
    temp_file="$(mktemp "${STATE_FILE}.tmp.XXXXXX")"

    awk -v provider="$PLATFORM" -v timestamp="$timestamp" '
        BEGIN { updated_provider=0; updated_context=0; updated_at=0; fm=0 }
        /^---$/ {
            fm++
            if (fm == 2) {
                if (!updated_provider) print "provider: " provider
                if (!updated_context) print "session_start_context_loaded: true"
                if (!updated_at) print "updated_at: " timestamp
            }
            print
            next
        }
        fm == 1 && /^provider:/ { print "provider: " provider; updated_provider=1; next }
        fm == 1 && /^session_start_context_loaded:/ { print "session_start_context_loaded: true"; updated_context=1; next }
        fm == 1 && /^updated_at:/ { print "updated_at: " timestamp; updated_at=1; next }
        { print }
    ' "$STATE_FILE" > "$temp_file"

    mv "$temp_file" "$STATE_FILE"
}

# ── Collect context from existing hooks ────────────────────────────────
# Run each hook and extract its context output (ignoring platform-specific
# wrapping). Each block follows the same pattern: run the Claude-shaped
# hook, then strip the platform wrapper so we can re-wrap with the
# cross-CLI shape at the bottom of this script.
#
# Order matters here: the resulting combined context is concatenated in
# the order these blocks fire, which is the order the agent reads at
# turn one. Keep this aligned with the SessionStart array in hooks.json
# so the Claude and non-Claude surfaces converge on the same preamble.

# 1. using-fno preamble — names both surfaces (slash commands + fno
#    CLI) and the HARD-GATE forbidden writes. Highest priority because
#    it teaches the agent the verbs the rest of the context will use.
using_fno_content=""
if [[ -f "${SCRIPT_DIR}/session-start-using-fno.sh" ]]; then
    raw_using_fno=$(bash "${SCRIPT_DIR}/session-start-using-fno.sh" 2>/dev/null || true)
    if [[ -n "$raw_using_fno" ]]; then
        using_fno_content=$(echo "$raw_using_fno" | jq -r '.hookSpecificOutput.additionalContext // .additional_context // .additionalContext // empty' 2>/dev/null || echo "$raw_using_fno")
    fi
fi

# 2. project vision — what this codebase is and why.
vision_content=""
if [[ -f "${SCRIPT_DIR}/inject-project-vision.sh" ]]; then
    raw_vision=$(bash "${SCRIPT_DIR}/inject-project-vision.sh" 2>/dev/null || echo "")
    if [[ -n "$raw_vision" ]]; then
        vision_content=$(echo "$raw_vision" | jq -r '.hookSpecificOutput.additionalContext // .additional_context // empty' 2>/dev/null || echo "$raw_vision")
    fi
fi

# 3. fno whoami — operational context for the current session
#    (fleet → walker → session stack, gates, provider). Helps the agent
#    re-orient after a fresh start or compaction.
whoami_content=""
if [[ -f "${SCRIPT_DIR}/inject-abi-agent-whoami.sh" ]]; then
    raw_whoami=$(bash "${SCRIPT_DIR}/inject-abi-agent-whoami.sh" 2>/dev/null || echo "")
    if [[ -n "$raw_whoami" ]]; then
        whoami_content=$(echo "$raw_whoami" | jq -r '.hookSpecificOutput.additionalContext // .additional_context // empty' 2>/dev/null || echo "$raw_whoami")
    fi
fi

# 4. worktree-scope hygiene heads-up — universal, advisory, never blocks.
#    Consumes the SAME shared verdict as /target, /do, /fix (one rule, no
#    drift). Emits at most two one-line notes and stays silent on a clean
#    worktree, a non-git dir, or any git error (the helper degrades to
#    verdict=ok / nested_count=0). This is the heads-up that today only
#    /target provides, surfaced for every implementation-capable thread
#    including `claude --bg`.
hygiene_content=""
hygiene_helper="${SCRIPT_DIR}/helpers/check-impl-location.sh"
if [[ -f "$hygiene_helper" ]]; then
    loc_out="$(bash "$hygiene_helper" 2>/dev/null || true)"
    if [[ -n "$loc_out" ]]; then
        loc_verdict="$(printf '%s\n' "$loc_out" | sed -n 's/^verdict=//p' | head -1)"
        loc_branch="$(printf '%s\n' "$loc_out" | sed -n 's/^branch=//p' | head -1)"
        loc_nested="$(printf '%s\n' "$loc_out" | sed -n 's/^nested_count=//p' | head -1)"
        hygiene_notes=""
        if [[ "$loc_verdict" == "canonical-protected" ]]; then
            hygiene_notes="- You are on canonical \`${loc_branch:-main}\` in the shared checkout. Implementation work should run in a worktree under \`~/conductor/workspaces/\` (sibling terminals share \`.fno/\`)."
        fi
        if [[ -n "$loc_nested" && "$loc_nested" != "0" ]]; then
            loc_path="$(printf '%s\n' "$loc_out" | sed -n 's/^nested_path=//p' | head -1)"
            nested_note="- Nested worktree present at \`${loc_path}\` (${loc_nested} total). \`grep -r\` descends into it; prefer \`rg\` / the Grep tool."
            if [[ -n "$hygiene_notes" ]]; then
                hygiene_notes="${hygiene_notes}"$'\n'"${nested_note}"
            else
                hygiene_notes="$nested_note"
            fi
        fi
        if [[ -n "$hygiene_notes" ]]; then
            hygiene_content="## Worktree hygiene"$'\n'"$hygiene_notes"
        fi
    fi
fi

# 4b. worktree HARNESS-ownership heads-up (x-193d Wave 5) — read-only consult of
#     the worktree claim. Warns when a DIFFERENT harness already owns this
#     worktree, before the PreToolUse guard would block the first write. Never
#     acquires here (--no-acquire) so a read-only session establishes nothing;
#     silent unless `fno` supports the verb AND a foreign owner exists.
if command -v fno >/dev/null 2>&1; then
    wt_guard_json="$(fno claim worktree-guard --no-acquire --json 2>/dev/null || true)"
    if [[ -n "$wt_guard_json" ]] && command -v jq >/dev/null 2>&1; then
        wt_verdict="$(printf '%s' "$wt_guard_json" | jq -er '.verdict | select(. != "") // empty' 2>/dev/null || true)"
        if [[ "$wt_verdict" == "foreign" ]]; then
            wt_owner="$(printf '%s' "$wt_guard_json" | jq -er '.owner_harness | select(. != "") // "another"' 2>/dev/null || echo another)"
            wt_note="- This worktree is owned by a \`${wt_owner}\` session; a second harness working it concurrently is refused at first write (x-193d). Use that session, a different worktree, or \`FNO_WORKTREE_OK=1\` to override."
            if [[ -n "$hygiene_content" ]]; then
                hygiene_content="${hygiene_content}"$'\n'"${wt_note}"
            else
                hygiene_content="## Worktree hygiene"$'\n'"${wt_note}"
            fi
        fi
    fi
fi

# 5. first-run setup nudge — points a brand-new user (no fno config yet) at the
#    setup wizard. Silent once any settings file exists. On Claude Code this
#    fires as its own hooks.json entry; here it rides the wrapper so Codex and
#    Gemini get the same nudge. The sub-hook prints plain markdown, so the jq
#    extraction falls through to the raw text (same as the vision block).
nudge_content=""
if [[ -f "${SCRIPT_DIR}/setup-nudge-session-start.sh" ]]; then
    raw_nudge=$(bash "${SCRIPT_DIR}/setup-nudge-session-start.sh" 2>/dev/null || echo "")
    if [[ -n "$raw_nudge" ]]; then
        nudge_content=$(printf '%s\n' "$raw_nudge" | jq -r '.hookSpecificOutput.additionalContext // .additional_context // empty' 2>/dev/null || printf '%s\n' "$raw_nudge")
    fi
fi

# 6. cross-harness mail drain (US5) — deliver this session's own a2a
#    mail. Drains the durable bus for messages addressed to this session's
#    <short-id> handle so a codex/gemini session RECEIVES `fno mail send`,
#    not just becomes addressable. Silent when empty / no harness identity.
mail_content=""
if [[ -f "${SCRIPT_DIR}/inject-mail-drain-session-start.sh" ]]; then
    mail_content=$(bash "${SCRIPT_DIR}/inject-mail-drain-session-start.sh" 2>/dev/null || echo "")
fi

# ── Combine context ───────────────────────────────────────────────────
# Newline-separate non-empty blocks so the agent sees each preamble as
# its own section rather than one wall of text.
combined=""
append_section() {
    local section="$1"
    [[ -z "$section" ]] && return 0
    if [[ -z "$combined" ]]; then
        combined="$section"
    else
        combined="${combined}"$'\n\n'"${section}"
    fi
}
append_section "$using_fno_content"
append_section "$vision_content"
append_section "$whoami_content"
append_section "$hygiene_content"
append_section "$nudge_content"
append_section "$mail_content"

# Self-heal a defunct target manifest (x-4af4) before anything reads it, so a
# dead target-state.md can no longer auto-lock an attended /think. Advisory.
gc_helper="${SCRIPT_DIR}/helpers/gc-dead-target-manifest.sh"
[[ -f "$gc_helper" ]] && bash "$gc_helper" "$STATE_FILE" || true

# Plan status reconcile sweep (x-f34f US5) — project canonical-but-stale plan
# frontmatter from graph truth. Daily-watermark gated and fully async so the
# 452-file parse never lands on the session-start critical path more than once a
# day; bg output is discarded so it can never corrupt this hook's JSON stdout.
# Watermark written BEFORE launch = at-most-once-per-day even if the sweep dies
# (the write-time projection and post-merge ritual are the other two layers).
if command -v fno >/dev/null 2>&1; then
    rs_watermark=".fno/.reconcile-status-watermark"
    rs_today="$(date +%Y-%m-%d 2>/dev/null || echo "")"
    if [[ -n "$rs_today" && "$(cat "$rs_watermark" 2>/dev/null || echo "")" != "$rs_today" ]]; then
        mkdir -p .fno 2>/dev/null; printf '%s\n' "$rs_today" >"$rs_watermark" 2>/dev/null || true
        ( fno plan reconcile-status --apply >/dev/null 2>&1 & ) 2>/dev/null || true
    fi
    # Grooming fallback (x-1c7b): fires only when the daily pass is >48h stale,
    # so a healthy LaunchAgent never trips it. Self-gates and self-watermarks.
    # Backgrounded whole: its freshness probe is a CLI shellout, and a healthy
    # machine never writes a watermark to skip it, so it would otherwise land on
    # every single session's critical path.
    gsh_helper="${SCRIPT_DIR}/helpers/groom-self-heal.sh"
    [[ -f "$gsh_helper" ]] && ( bash "$gsh_helper" >/dev/null 2>&1 & ) 2>/dev/null || true
    # Graph->doc mirror sweep: bare `plan sync` self-gates on graph.json mtime
    # (one stat, cheap on no change), so no shell watermark; backgrounded + output
    # discarded so it can never corrupt this hook's JSON.
    ( fno plan sync >/dev/null 2>&1 & ) 2>/dev/null || true
fi

hydrate_state_provider_context

# No context to inject — exit silently
if [[ -z "$combined" ]]; then
    exit 0
fi

# ── Platform-specific output ──────────────────────────────────────────
# Use jq for safe JSON escaping of the combined context string.
case "$PLATFORM" in
    claude|gemini|codex)
        # Claude, Gemini, and Codex have all converged on the same SessionStart
        # output contract: hookSpecificOutput.additionalContext (camelCase).
        # Verified against Gemini's hook reference (geminicli.com/docs/hooks/
        # reference) and Codex's SessionStartHookSpecificOutputWire schema. The
        # earlier `additional_context` (snake_case) shape for non-Claude was
        # stale and would be ignored by current Gemini/Codex.
        jq -n --arg ctx "$combined" \
            '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":$ctx}}'
        ;;
    *)
        # Cursor / generic — unverified; keep the legacy additional_context shape.
        jq -n --arg ctx "$combined" \
            '{"additional_context":$ctx}'
        ;;
esac

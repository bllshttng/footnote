#!/usr/bin/env bash
# claude-config-write-guard.sh - PreToolUse hook: refuse a write or redirect
# to a file DIRECTLY inside the Claude config dir (~/.claude, or
# CLAUDE_CONFIG_DIR), top level only, and refuse a NEW top-level entry in the
# fno state root (${FNO_STATE_DIR:-${FNO_HOME:-~/.fno}}). Agents
# scratch-littering the config dir (ab.out, uvsync.out, agents-ci.log) dropped
# the jobs/<id>/tmp segment; the refusal names the session's job tmp dir as
# the place to write.
#
# A RELATIVE write target resolves against the payload cwd before the check:
# the measured incident was a worker whose cwd WAS the config dir running
# `> chk1.out`, which names no config-dir token. An existing top-level entry
# in the state root stays writable (it is one of the inventory's rows); a new
# one is refused whatever its name (state-root tidiness), and the refusal
# names the job tmp dir.
#
# jobs/, projects/, plugins/ and every other subdirectory stay allowed. Named
# harness config files stay allowed too (_keeplisted): the update-config and
# keybindings skills legitimately edit them.
#
# A payload naming no config-dir or state-root token is approved without
# parsing (fast path). A payload that DOES name one but cannot be parsed is
# approved too, deliberately fail-open: unlike graph-write-protect there is
# no forge or state-file backstop here, and a wedged guard starves every
# session on the machine. The reclaim lane claude_config_tmp is the backstop
# for litter this misses.
#
# Exit 0 always (hook result is communicated via stdout JSON).
set -uo pipefail

# Survive a caller env with no usable PATH hardening (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

_HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/guard-mark.sh
source "$_HOOK_DIR/lib/guard-mark.sh" 2>/dev/null || true

_approve() {
    _guard_mark claude-config-write-guard allow 2>/dev/null || true
    printf '%s\n' '{}'
    exit 0
}

_block() {
    _guard_mark claude-config-write-guard block 2>/dev/null || true
    local reason="$1"
    if command -v jq >/dev/null 2>&1; then
        jq -nc --arg reason "$reason" '{
            decision: "block",
            reason: $reason,
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "deny",
                permissionDecisionReason: $reason
            }
        }'
    else
        python3 -c 'import json,sys; r=sys.argv[1]; print(json.dumps({"decision":"block","reason":r,"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":r}}))' "$reason"
    fi
    exit 0
}

# ── 1. Cheap pre-filter: no config-dir or state-root token anywhere ──────────
# When CLAUDE_CONFIG_DIR points somewhere not named *.claude*, its paths name
# no token this filter can see, so the fast path is skipped and the parse
# decides.
PAYLOAD="$(cat)"
prefilter_ok=1
if [[ -n "${CLAUDE_CONFIG_DIR:-}" && "$CLAUDE_CONFIG_DIR" != *".claude"* ]]; then
    prefilter_ok=0
fi
# A relocated state root names no visible token; the parse decides.
if [[ -n "${FNO_STATE_DIR:-}" || -n "${FNO_HOME:-}" ]]; then
    prefilter_ok=0
fi
if [[ "$prefilter_ok" == 1 && "$PAYLOAD" != *".claude"* && "$PAYLOAD" != *"CLAUDE_CONFIG_DIR"* \
    && "$PAYLOAD" != *".fno"* && "$PAYLOAD" != *"FNO_STATE_DIR"* && "$PAYLOAD" != *"FNO_HOME"* ]]; then
    _approve
fi

# ── 2. Parse the payload (jq -> python3 → fail OPEN) ────────────────────────
TOOL="" FILE_PATH="" COMMAND="" SESSION_ID="" CWD=""
if command -v jq >/dev/null 2>&1; then
    { read -r TOOL; read -r FILE_PATH; read -r COMMAND; read -r SESSION_ID; read -r CWD; } < <(
        printf '%s' "$PAYLOAD" | jq -r '
            def norm: gsub("/+";"/") | gsub("/\\./";"/");
            .tool_name // "",
            (.tool_input.file_path // "" | norm),
            (.tool_input.command // "" | gsub("\n";" ") | norm),
            (.session_id // ""),
            (.cwd // "")' 2>/dev/null
    )
elif command -v python3 >/dev/null 2>&1; then
    { read -r TOOL; read -r FILE_PATH; read -r COMMAND; read -r SESSION_ID; read -r CWD; } < <(
        printf '%s' "$PAYLOAD" | python3 -c '
import sys, json, re
def norm(s): return re.sub(r"/\./", "/", re.sub(r"/+", "/", s))
try:
    d = json.load(sys.stdin); ti = d.get("tool_input") or {}
    print(d.get("tool_name") or ""); print(norm(ti.get("file_path") or ""))
    print(norm((ti.get("command") or "").replace("\n", " "))); print(d.get("session_id") or "")
    print(norm(d.get("cwd") or ""))
except Exception:
    pass' 2>/dev/null
    )
else
    _approve
fi
# Fail-open on an unparsable payload that still named a token (see header).
[[ -n "$TOOL" ]] || _approve

# ── 3. Config dirs, state root, keep-list ────────────────────────────────────
AMBIENT="${HOME%/}/.claude"
CFGS=("$AMBIENT")
if [[ -n "${CLAUDE_CONFIG_DIR:-}" && "${CLAUDE_CONFIG_DIR%/}" != "$AMBIENT" ]]; then
    CFGS+=("${CLAUDE_CONFIG_DIR%/}")
fi
# The fno state root, resolved like git-protection.py: config-free carriers,
# then the ambient default. An existing top-level entry stays writable; a NEW
# one is refused (nothing writes at the top level of either root).
STATE="${FNO_STATE_DIR:-${FNO_HOME:-${HOME%/}/.fno}}"
STATE="${STATE%/}"

# _physical ABS -> ABS with its nearest existing ancestor resolved physically,
# so a symlinked config dir compares equal to the path a command writes.
_physical() {
    local dir="$1" rest=""
    while [[ ! -d "$dir" ]]; do
        rest="/${dir##*/}$rest"
        dir="${dir%/*}"
        [[ -n "$dir" ]] || dir="/"
    done
    printf '%s%s\n' "$(cd -P "$dir" 2>/dev/null && pwd -P)" "$rest"
}

PHYS_CFGS=()
for cfg in "${CFGS[@]}"; do
    PHYS_CFGS+=("$(_physical "$cfg")")
done
PHYS_STATE="$(_physical "$STATE")"

# _keeplisted NAME -> harness config files an agent may still edit in place.
_keeplisted() {
    case "$1" in
        settings.json | settings.local.json | keybindings.json | CLAUDE.md) return 0 ;;
        *) return 1 ;;
    esac
}

# _abs_of TEXT -> absolute path for a write-operator match: quotes stripped,
# ~, $HOME and $CLAUDE_CONFIG_DIR expanded, a RELATIVE token resolved against
# the payload cwd, then physicalized. The regex families hand it the token
# they saw in the command text. A relative token with no payload cwd stays
# unresolved (empty result): guessing the hook's own cwd would misresolve.
_abs_of() {
    local t="$1"
    t="${t//\"/}"
    t="${t//\`/}"
    case "$t" in
        /*) ;;
        \~*) t="${HOME%/}${t#\~}" ;;
        \$HOME*) t="${t/\$HOME/${HOME%/}}" ;;
        \$CLAUDE_CONFIG_DIR*) t="${t/\$CLAUDE_CONFIG_DIR/${CLAUDE_CONFIG_DIR:-$AMBIENT}}" ;;
        *)
            if [[ -n "$CWD" ]]; then
                t="${CWD%/}/$t"
            else
                t=""
            fi
            ;;
    esac
    [[ -n "$t" ]] || return
    _physical "$t"
}

# _jobdir_for CFG -> the session's job tmp dir under that namespace. The
# payload's own session outranks any inherited CLAUDE_JOB_DIR: an exported
# env can belong to a different session than the one gated.
_jobdir_for() {
    local cfg="$1"
    if [[ -n "$SESSION_ID" ]]; then
        printf '%s\n' "${cfg}/jobs/${SESSION_ID:0:8}/tmp"
    elif [[ -n "${CLAUDE_JOB_DIR:-}" ]]; then
        printf '%s\n' "${CLAUDE_JOB_DIR%/}/tmp"
    else
        printf '%s\n' "${cfg}/jobs/<session-id>/tmp"
    fi
}

# _refuse_for ABS -> blocks when ABS sits directly inside a config dir (or is
# a config dir itself: `cp staged ~/.claude/` lands a top-level file), outside
# the keep-list and the harness-owned dotfiles; or when ABS would CREATE a
# new top-level entry in the state root (an existing row stays writable). The
# refusal names a job tmp dir under the VIOLATED namespace, so an
# isolated-account write is never pointed at the ambient one.
_refuse_for() {
    local abs="$1" name phys cfg i jobdir
    [[ -n "$abs" ]] || return 1
    name="${abs##*/}"
    for i in "${!PHYS_CFGS[@]}"; do
        phys="${PHYS_CFGS[$i]}"
        cfg="${CFGS[$i]}"
        if [[ "$abs" == "$phys" || "$abs" == "$phys"/* && "$abs" != "$phys"/*/* ]]; then
            if _keeplisted "$name" || [[ "$name" == .claude.json* ]]; then
                return 1
            fi
            jobdir="$(_jobdir_for "$cfg")"
            _block "$abs is a write directly inside the Claude config dir ($cfg). Scratch belongs in this session's job dir: $jobdir. Subdirectories (jobs/, projects/, plugins/) stay allowed."
        fi
    done
    if [[ "$abs" == "$PHYS_STATE" ]]; then
        jobdir="$(_jobdir_for "$AMBIENT")"
        _block "$abs is the fno state root itself; a copy there lands a new top-level file. State belongs in a named subfolder under it, or this session's job dir: $jobdir."
    elif [[ "$abs" == "$PHYS_STATE"/* && "$abs" != "$PHYS_STATE"/*/* && ! -e "$abs" ]]; then
        jobdir="$(_jobdir_for "$AMBIENT")"
        _block "$abs would create a new top-level entry in the fno state root ($STATE). Nothing writes at the top level: put state in a named subfolder under it, or this session's job dir: $jobdir."
    fi
    return 1
}

# ── 4. Bash write-operator families (same posture as graph-write-protect) ────
# A path-shaped token counts only where a write operator binds it; a bare
# mention or a read never matches. First match per family is the documented
# enumerated floor, not Turing-complete coverage.
name_cls='[^[:space:];|&<>)]+'   # the FULL bound path, slashes included:
                                # top-level-ness is decided on the resolved
                                # path in _refuse_for, not here
# _re_quote TEXT -> every ERE metacharacter escaped, so a runtime path value
# (HOME, CLAUDE_CONFIG_DIR) reads as a literal inside the arms.
_re_quote() {
    printf '%s' "$1" | sed -e 's/[][\.*^$()+?{|}]/\\&/g'
}
arms=('\$CLAUDE_CONFIG_DIR' '\$HOME/\.claude' '~/\.claude')
home_esc="$(_re_quote "${HOME%/}")"
arms+=("${home_esc}/\.claude")
if [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
    arms+=("$(_re_quote "${CLAUDE_CONFIG_DIR%/}")")
fi
path_arm=""
for arm in "${arms[@]}"; do
    [[ -n "$path_arm" ]] && path_arm+='|'
    path_arm+="$arm"
done
pp="((${path_arm})(/${name_cls})?)($|[/[:space:];|&:)])"
# The boundary group after the path keeps the arm from matching a proper
# prefix of a SIBLING name (~/.claude.json, ~/.claude-alt): without it the
# bare-dir form would read any .claude* file at \$HOME as the directory.
# rpp is the RELATIVE twin: a write operator binding a relative path, which
# _abs_of resolves against the payload cwd. Same group shape as pp, so each
# family check reads the same BASH_REMATCH index either way.
rpp="((\./)?${name_cls})($|[/[:space:];|&:)])"
op='([>]{1,2}|\&[>]|[>]\&|[>][|]|[>]!)'
nosep='[^;|&]*'

case "$TOOL" in
    Edit | Write)
        [[ -n "$FILE_PATH" ]] || _approve
        abs="$FILE_PATH"
        case "$abs" in
            /*) ;;
            \~*) abs="${abs/#\~/${HOME%/}}" ;;
            *)
                # Same rule as the Bash arm: a relative target with no
                # payload cwd stays unresolved rather than resolving
                # against the hook process's unrelated directory.
                if [[ -n "$CWD" ]]; then
                    abs="${CWD%/}/$abs"
                else
                    abs=""
                fi
                ;;
        esac
        abs="$(_physical "$abs")"
        _refuse_for "$abs" || true
        _approve
        ;;
    Bash)
        [[ -n "$COMMAND" ]] || _approve
        # redirect immediately targeting the path (absolute, then relative)
        if [[ "$COMMAND" =~ ${op}[[:space:]]*${pp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[2]}")" || true
        fi
        if [[ "$COMMAND" =~ ${op}[[:space:]]*${rpp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[2]}")" || true
        fi
        # tee [flags] path / sponge path
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])(tee|sponge)[[:space:]]+(-[^[:space:]]+[[:space:]]+)*${pp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[4]}")" || true
        fi
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])(tee|sponge)[[:space:]]+(-[^[:space:]]+[[:space:]]+)*${rpp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[4]}")" || true
        fi
        # cp / mv / install / truncate with the path as the (last) argument
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])(cp|mv|install|truncate)[[:space:]].*[[:space:]]${pp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[3]}")" || true
        fi
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])(cp|mv|install|truncate)[[:space:]].*[[:space:]]${rpp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[3]}")" || true
        fi
        # dd of=path
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])dd[[:space:]].*of=${pp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[2]}")" || true
        fi
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])dd[[:space:]].*of=${rpp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[2]}")" || true
        fi
        # in-place editors, clause-bounded like graph-write-protect
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])(sed|perl|jq|ex|ed)[[:space:]]${nosep}(-[a-zA-Z]*i|--in-place)${nosep}${pp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[4]}")" || true
        fi
        if [[ "$COMMAND" =~ (^|[^[:alnum:]_])(sed|perl|jq|ex|ed)[[:space:]]${nosep}(-[a-zA-Z]*i|--in-place)${nosep}${rpp} ]]; then
            _refuse_for "$(_abs_of "${BASH_REMATCH[4]}")" || true
        fi
        _approve
        ;;
    *)
        _approve
        ;;
esac

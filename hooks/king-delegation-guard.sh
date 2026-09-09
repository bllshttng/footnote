#!/usr/bin/env bash
# king-delegation-guard.sh - PreToolUse hook: a crowned court session does not
# implement (operator directive point 5). Seven existing PreToolUse guards
# guard a THING (the graph file, a location, destructive git); this one guards
# a ROLE: a session whose registry row carries a crown and whose reign
# manifest declares shape "court" is refused Edit/Write/NotebookEdit and
# shell writes to source. A "pass" session writes its own wave plan and
# abdicates, so it is not a court and stays silent here.
#
# The unblock authority (directive point 6) is an allowlist read by eye, not
# a judgement per call: fno agents claim, fno agents mail, the backlog levers
# (encounter, update, undefer, supersede, note, advance, rank), fno inbox,
# fno doctor event emit, and any write whose path resolves inside the plans
# directory (king-for-a-day authors quick plans for small in-scope nodes;
# plan-location-guard polices where those land). A bare `fno ...` verb needs
# no carveout: the Bash branch keys on shell write operators, and a verb that
# writes state internally binds no redirect.
#
# NEVER blocks by accident. Any failure to read the payload, the registry,
# the manifest, or the config exits 0 and allows - the compact-hook contract
# (a guard that refuses because it could not read something is a king that
# cannot act). The knob config.king.implementation_guard takes refuse
# (default), warn or off; warn emits the refusal text on stderr and allows.
#
# Exit 0 always (hook result is communicated via stdout JSON).
# Known ceiling: the Bash write floor enumerates shell write operators, so a
# writer hidden inside `python3 -c` is not caught - the same accepted floor
# the graph guard documents; the operator's own review is the backstop.
set -uo pipefail

SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${FNO_PLATFORM:-}" == "codex" ]]; then
  PLUGIN_ROOT="${CODEX_PLUGIN_ROOT:-${PLUGIN_ROOT:-$SOURCE_ROOT}}"
else
  PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$SOURCE_ROOT}}"
fi
# shellcheck source=hooks/lib/guard-mark.sh
source "${SOURCE_ROOT}/hooks/lib/guard-mark.sh" 2>/dev/null || true

_approve() {
    _guard_mark king-delegation-guard allow 2>/dev/null || true
    printf '%s\n' '{}'
    exit 0
}
_deny_text() {
    printf '%s\n' \
"king-delegation-guard: a crowned court session does not implement (directive point 5).
Delegate it: fno agents spawn --node <id> --substrate bg
Or hand the whole scope out: fno backlog advance --epic <scope>
Unblock authority is yours and is allowed: claim release, mail, backlog levers, notes, plan writes."
}
_block() {
    _guard_mark king-delegation-guard block 2>/dev/null || true
    local r
    r="$(_deny_text)"
    jq -n --arg r "$r" '{
        decision: "block",
        reason: $r,
        hookSpecificOutput: {
            hookEventName: "PreToolUse",
            permissionDecision: "deny",
            permissionDecisionReason: $r
        }
    }' 2>/dev/null || printf '{"decision":"block","reason":"%s","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$r" "$r"
    exit 0
}

# ── 1. Payload parse (fail-open: an unreadable payload is not a refusal) ──────
PAYLOAD=""
if [[ ! -t 0 ]]; then
    PAYLOAD="$(cat 2>/dev/null || true)"
fi
if [[ -z "$PAYLOAD" ]]; then
    _approve
fi
TOOL="" FILE_PATH="" COMMAND="" SID="" TRANSCRIPT="" CWD=""
{
    read -r TOOL
    read -r FILE_PATH
    read -r COMMAND
    read -r SID
    read -r TRANSCRIPT
    read -r CWD
} < <(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    e = json.load(sys.stdin)
except Exception:
    e = {}
ti = e.get("tool_input") or {}
print(e.get("tool_name") or "")
print(ti.get("file_path") or ti.get("notebook_path") or "")
print((ti.get("command") or "").replace("\n", " "))
print(e.get("session_id") or "")
print(e.get("transcript_path") or "")
print(e.get("cwd") or "")
' 2>/dev/null) || true
if [[ -z "$TOOL" ]]; then
    _approve
fi

case "$TOOL" in
  Edit|Write|NotebookEdit|Bash) ;;
  *) _approve ;;
esac

# ── 2. This session's id (shared resolver: event field -> transcript basename
#      -> harness env markers). Unresolvable: not ours to judge, allow. ────────
# shellcheck source=scripts/lib/postcompact-carrier.sh
source "${PLUGIN_ROOT}/scripts/lib/postcompact-carrier.sh" 2>/dev/null || true
if declare -F postcompact_resolve_sid >/dev/null 2>&1; then
    SID="$(postcompact_resolve_sid "$SID" "$TRANSCRIPT")"
fi
if [[ -z "$SID" ]]; then
    _approve
fi

# ── 3. Registry row: crown_level or crown_scope marks a crowned session. ─────
# `fno agents registry-json` is a daemon-free file read (never `fno agents
# list`, which lazy-starts the daemon). Unreadable -> stderr + allow; no row
# -> silent allow (the common case for every ordinary session).
command -v fno >/dev/null 2>&1 || { echo "king-delegation-guard: fno unreadable; allowing" >&2; _approve; }
command -v jq  >/dev/null 2>&1 || { echo "king-delegation-guard: jq unreadable; allowing" >&2; _approve; }
AGENTS_JSON="$(fno agents registry-json 2>/dev/null || true)"
if [[ -z "$AGENTS_JSON" ]]; then
    echo "king-delegation-guard: registry row unreadable; allowing" >&2
    _approve
fi
MY_ROW="$(printf '%s' "$AGENTS_JSON" | jq -c --arg sid "$SID" \
    '.agents[] | select(.session_id == $sid or .harness_session_id == $sid)' 2>/dev/null | head -1)"
if [[ -z "$MY_ROW" ]]; then
    _approve
fi
CROWN_LEVEL="$(printf '%s' "$MY_ROW" | jq -r '.crown_level // empty' 2>/dev/null)"
CROWN_SCOPE="$(printf '%s' "$MY_ROW" | jq -r '.crown_scope // empty' 2>/dev/null)"
if [[ -z "$CROWN_LEVEL" && -z "$CROWN_SCOPE" ]]; then
    _approve
fi

# ── 4. Shape: the reign manifest is the declaration (rides from birth). ──────
# Same manifest every king arm resolves; a missing manifest or a foreign
# session id means no court is declared here, so the guard stays silent.
REPO_ROOT="$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
REIGN_MANIFEST="$(fno agents king manifest-path --harness-session-id "$SID" \
    --state-root "$REPO_ROOT/.fno" 2>/dev/null || true)"
if [[ -z "$REIGN_MANIFEST" || ! -f "$REIGN_MANIFEST" ]]; then
    _approve
fi
REIGN_SHAPE="$(sed -n 's/^shape:[[:space:]]*//p' "$REIGN_MANIFEST" | head -1 | tr -d '[:space:]')"
REIGN_SID="$(sed -n 's/^harness_session_id:[[:space:]]*//p' "$REIGN_MANIFEST" | head -1 | tr -d '[:space:]')"
if [[ "$REIGN_SHAPE" != "court" || "$REIGN_SID" != "$SID" ]]; then
    _approve
fi

# ── 5. Knob: refuse (default) | warn | off. Unreadable or unknown degrades
#      to refuse, the deliberate default. ─────────────────────────────────────
MODE="$(fno config get king.implementation_guard 2>/dev/null || true)"
case "$MODE" in
  off)  _approve ;;
  warn) _deny_text >&2
        _approve ;;
  *)    : ;;  # refuse, and any value that is not warn/off
esac

# ── 6. Plans-directory carveout: writes that resolve inside the resolved
#      plans dir are allowed (quick plans stay in-lane). ──────────────────────
in_plans_dir() {
    local p="$1"
    [[ -n "$p" ]] || return 1
    printf '%s' "$p" | python3 -c '
import os, sys
p, cwd, d = sys.argv[1], sys.argv[2], sys.argv[3]
if not os.path.isabs(p):
    p = os.path.join(cwd or os.getcwd(), p)
p = os.path.normpath(p)
sys.exit(0 if (p == d or p.startswith(d + os.sep)) else 1)
' "$p" "$CWD" "$PLANS_DIR" 2>/dev/null
}
PLANS_DIR="$(fno do plan path --slug delegation-guard-probe 2>/dev/null | sed 's|/[^/]*$||')"
if [[ -z "$PLANS_DIR" ]]; then
    # The resolver is unreadable; the carveout cannot be honored, so the
    # never-block contract allows rather than refusing a legal plan write.
    _approve
fi

# ── 7. Decision ───────────────────────────────────────────────────────────────
case "$TOOL" in
  Edit|Write|NotebookEdit)
    if in_plans_dir "$FILE_PATH"; then
        _approve
    fi
    _block
    ;;
  Bash)
    # Floor of shell write operators (redirect, tee, sponge, cp/mv/install/
    # truncate, dd of=, in-place sed/perl, ed/ex); each bound target must land
    # inside the plans dir. A verb with no redirect binds nothing and allows.
    WRITTEN="$(printf '%s' "$COMMAND" | python3 -c '
import re, sys
cmd = sys.stdin.read().strip()
paths = []
nosep = r"[^\s;|&<>]*"
# redirects: > >> 2> &> >& >| >! then the target path
for m in re.finditer(r"(?:[>]{1,2}|&[>]|[>]&|[>][|]|[>]!)\s*(" + nosep + r")", cmd):
    paths.append(m.group(1))
# tee / sponge / truncate with the target as the (last) argument
for verb in ("tee", "sponge", "truncate"):
    for m in re.finditer(r"(?:^|[^\w])" + verb + r"\s+(?:-[^\s]+\s+)*(" + nosep + r")", cmd):
        paths.append(m.group(1))
# cp / mv / install: target is the final argument of the clause
for m in re.finditer(r"(?:^|[^\w])(?:cp|mv|install)\s+[^;|&]+?[\s]+(" + nosep + r")(?:[;|&]|$)", cmd):
    paths.append(m.group(1))
# dd of=path
for m in re.finditer(r"(?:^|[^\w])dd\s+[^;|&]*?of=(" + nosep + r")", cmd):
    paths.append(m.group(1))
# in-place editors: -i / -pi / --in-place bound to a path in the same clause
for m in re.finditer(r"(?:^|[^\w])(?:sed|perl)\s+[^;|&]*?(?:-[a-zA-Z]*i|--in-place)\s+(?:-e\s+\S+\s+)*(" + nosep + r")", cmd):
    paths.append(m.group(1))
for m in re.finditer(r"(?:^|[^\w])(?:ed|ex)\s+(" + nosep + r")", cmd):
    paths.append(m.group(1))
for p in paths:
    if p:
        print(p)
' 2>/dev/null)"
    ALLOW=1
    while IFS= read -r p; do
        [[ -n "$p" ]] || continue
        if ! in_plans_dir "$p"; then
            ALLOW=0
            break
        fi
    done <<< "$WRITTEN"
    if [[ "$ALLOW" -eq 1 ]]; then
        _approve
    fi
    _block
    ;;
  *)
    _approve
    ;;
esac

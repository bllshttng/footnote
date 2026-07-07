#!/usr/bin/env bash
# graph-write-protect.sh - PreToolUse hook: block writes to the two forbidden
# state files ~/.fno/graph.json and .fno/target-state.md across Edit, Write,
# AND Bash tools (x-4c48: close the Bash bypass + fail-closed parse + general
# manifest immutability).
#
# Flow (design x-4c48):
#   1. jq-free substring pre-filter: if neither protected path token appears in
#      the raw payload, approve fast. This never calls jq, so a missing jq can
#      no longer fail OPEN (old finding b), and normal edits pay ~zero cost.
#   2. A protected token IS present -> parse precisely with jq, else python3,
#      else FAIL CLOSED (block) on this narrow branch only.
#   3. Tool-specific decision keyed on the write TARGET, never on payload
#      substring (so editing AGENTS.md - which mentions the paths - is allowed):
#        Edit|Write -> block graph.json / target-state.md by file_path suffix
#        Bash       -> block only when a write-op is bound to a protected path
#
# target-state.md is immutable to Edit/Write UNCONDITIONALLY (finding c): its
# only legitimate writers are Bash verbs (`fno target init`, `fno state set
# --field plan_path`) that carry no path+redirect, so a flat block has zero
# legitimate collateral. This removes the forgeable trust root the user-global
# merge gate (git-protection.py) reads (auto_merge_approved). The drive-window
# gate-diff branch is deleted as dead complexity; we keep the forensic audit
# event when a manifest write lands during a drive.
#
# The .fno/artifacts/*.md drive-window allow cell (cv-9def52a7) is preserved.
#
# Exit 0 always (hook result is communicated via stdout JSON).
set -uo pipefail

_HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_REPO_ROOT="$(cd "${_HOOK_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib/drive-authority.sh
source "${_REPO_ROOT}/scripts/lib/drive-authority.sh" 2>/dev/null || true
# shellcheck source=../scripts/lib/events.sh
source "${_REPO_ROOT}/scripts/lib/events.sh" 2>/dev/null || true

# Fail-open shim: if drive-authority.sh did not load, report "no window" so a
# missing lib never blocks an ordinary edit and the audit path degrades cleanly.
if ! declare -F drive_authority_active >/dev/null 2>&1; then
    drive_authority_active() { return 1; }
fi

_approve() { printf '%s\n' '{"decision": "approve"}'; exit 0; }
_block()   { jq -n --arg r "$1" '{"decision":"block","reason":$r}' 2>/dev/null \
                || printf '{"decision":"block","reason":"%s"}\n' "$1"; exit 0; }

# _bash_targets_protected CMD -> return 0 if a write operator in CMD is bound to
# a protected path (.fno/graph.json or .fno/target-state.md), else 1. Keyed on
# operator+path adjacency, not bare mention: `echo "see .fno/graph.json" >> x`
# writes x (no match); `cat .fno/graph.json` reads (no match). Enumerated floor
# per design x-4c48; not Turing-complete coverage (merge-gate artifact backstop).
_bash_targets_protected() {
    local cmd="$1"
    # A protected-path token. The prefix (leading dir/`~`/`$HOME`/quote chars up
    # to `.fno/`) excludes only whitespace and command separators/redirects, NOT
    # quotes - a quoted path like `> "$HOME/.fno/graph.json"` is normal shell and
    # must still match (codex P1). Right-bounded by a shell separator/quote or EOL.
    local pp='[^[:space:];|&<>]*\.fno/(graph\.json|target-state\.md)([[:space:];|&<>"'\'']|$)'
    # A run of non-separator chars (stays inside one command clause), and a
    # clause tail that ends at a protected path. Kept in vars because an inline
    # `[^;|&]` breaks `[[ =~ ]]` parsing (`;`/`|` are shell-special there).
    local nosep='[^;|&]*'
    local clause="${nosep}"'\.fno/(graph\.json|target-state\.md)'
    # redirect immediately targeting the path: >, >>, 2>, &>, >&, >|, >!
    # (bracket forms, not \>, to avoid the GNU word-boundary reading of \>).
    [[ "$cmd" =~ ([>]{1,2}|\&[>]|[>]\&|[>][|]|[>]!)[[:space:]]*$pp ]] && return 0
    # tee [flags] path  (also `... | tee path`)
    [[ "$cmd" =~ (^|[^[:alnum:]_])tee[[:space:]]+(-[^[:space:]]+[[:space:]]+)*$pp ]] && return 0
    # sponge path
    [[ "$cmd" =~ (^|[^[:alnum:]_])sponge[[:space:]]+$pp ]] && return 0
    # cp / mv / install / truncate with the protected path as the (last) argument
    [[ "$cmd" =~ (^|[^[:alnum:]_])(cp|mv|install|truncate)[[:space:]].*[[:space:]]$pp ]] && return 0
    # dd of=path
    [[ "$cmd" =~ (^|[^[:alnum:]_])dd[[:space:]].*of=$pp ]] && return 0
    # in-place editors: bind the editor to the protected path WITHIN its own
    # command clause (codex P2) - `echo see .fno/graph.json; sed -i x notes.md`
    # mentions the path in a different clause and must NOT match. `[^;|&]*`
    # keeps the flag and the path in the same clause as the sed/perl/jq/ed verb.
    # -i, combined short flags (-Ei, -ri), and the --in-place long form.
    local re_sed="(^|[^[:alnum:]_])(sed|perl)[[:space:]]${nosep}(-[a-zA-Z]*i|--in-place)${clause}"
    local re_jq="(^|[^[:alnum:]_])jq[[:space:]]${nosep}(-i|--in-place)${clause}"
    local re_ed="(^|[^[:alnum:]_])(ex|ed)[[:space:]]${clause}"
    [[ "$cmd" =~ $re_sed ]] && return 0
    [[ "$cmd" =~ $re_jq  ]] && return 0
    [[ "$cmd" =~ $re_ed  ]] && return 0
    return 1
}

PAYLOAD=$(cat)

# ── 1. jq-free pre-filter ──────────────────────────────────────────────────────
# Key on the bare protected FILENAMEs, not the full `.fno/<file>` token. These
# names are rare enough to stay selective, and matching the filename alone is
# tolerant of every separator/escaping variant that writes the same file - a
# JSON-escaped slash (`.fno\/graph.json`), a doubled slash (`.fno//graph.json`),
# a dot segment (`.fno/./graph.json`), or a quoted path all still contain the
# filename (codex P1). If NEITHER filename appears, the call cannot target a
# protected file: approve fast without calling jq. Over-match is safe: the
# precise parse + normalization below keys on the write TARGET.
if [[ "$PAYLOAD" != *"graph.json"* && "$PAYLOAD" != *"target-state.md"* ]]; then
    _approve
fi

# ── 2. Precise parse (jq -> python3 -> fail closed) ────────────────────────────
# One parser invocation, three newline-separated fields. The parser also
# NORMALIZES the file_path/command slashes (`/+`->`/`, `/./`->`/`) so a
# separator-equivalent path like `~/.fno//graph.json` or `.fno/./target-state.md`
# resolves to the canonical form the checks below key on (codex P1). Command
# newlines are flattened to spaces so the third `read` gets it on one line.
TOOL="" FILE_PATH="" COMMAND=""
if command -v jq >/dev/null 2>&1; then
    { read -r TOOL; read -r FILE_PATH; read -r COMMAND; } < <(printf '%s' "$PAYLOAD" | jq -r '
        def norm: gsub("/+";"/") | gsub("/\\./";"/");
        .tool_name // "",
        (.tool_input.file_path // "" | norm),
        (.tool_input.command // "" | gsub("\n";" ") | norm)' 2>/dev/null)
elif command -v python3 >/dev/null 2>&1; then
    { read -r TOOL; read -r FILE_PATH; read -r COMMAND; } < <(printf '%s' "$PAYLOAD" | python3 -c '
import sys, json, re
def norm(s): return re.sub(r"/\./", "/", re.sub(r"/+", "/", s))
try:
    d = json.load(sys.stdin); ti = d.get("tool_input") or {}
    print(d.get("tool_name") or ""); print(norm(ti.get("file_path") or ""))
    print(norm((ti.get("command") or "").replace("\n", " ")))
except Exception:
    pass' 2>/dev/null)
else
    # A protected token is present but no parser exists to resolve the target.
    # Fail CLOSED (old finding b was the opposite: parser-absence fail-open).
    _block "graph-write-protect: neither jq nor python3 available to parse a payload referencing a protected state file; blocking fail-closed. Install jq or python3."
fi

# Malformed payload that still bears a protected token: parsing yielded no tool.
# Fail closed rather than approve a possible forge (Failure Modes: block when a
# protected token is literally present and parsing fails).
if [[ -z "$TOOL" ]]; then
    _block "graph-write-protect: payload references a protected state file but could not be parsed; blocking fail-closed."
fi

_GRAPH_REASON="graph.json must be mutated via \`fno backlog\` commands; direct write blocked. See \`fno backlog --help\` (add, idea, intake, update, done, defer, reconcile)."
_MANIFEST_REASON="target-state.md is an immutable session manifest; direct Edit/Write is blocked. The only legal post-init write is first-fill of an empty plan_path via \`fno state set --field plan_path\`. Use \`fno state\` / \`fno target\` verbs, not a hand edit."

# ── 3. Tool-specific decision (keyed on the write TARGET) ──────────────────────
case "$TOOL" in
  Edit|Write)
    # Fixture/test scaffolding may hold a graph.json/target-state.md under a
    # test dir; those are editable.
    if [[ "$FILE_PATH" == *"/test/"* || "$FILE_PATH" == *"/tests/"* || "$FILE_PATH" == *"/fixtures/"* ]]; then
        _approve
    fi
    if [[ "$FILE_PATH" == *".fno/graph.json" ]]; then
        _block "$_GRAPH_REASON"
    fi
    if [[ "$FILE_PATH" == *".fno/target-state.md" ]]; then
        # Finding c: block unconditionally (not drive-window-only). Emit the
        # forensic audit event if this forge lands during a drive; block regardless.
        if drive_authority_active && declare -F emit_event >/dev/null 2>&1; then
            emit_event "hook" "gate_edit_forged_during_drive" \
                "$(jq -nc --arg fp "$FILE_PATH" '{file_path:$fp, reason:"drive_authority_active"}' 2>/dev/null || echo '{}')" \
                2>/dev/null || true
        fi
        _block "$_MANIFEST_REASON"
    fi
    # cv-9def52a7: artifact edit during a drive -> ALLOWED, audit-tagged.
    if [[ "$FILE_PATH" == *"/.fno/artifacts/"*.md ]] && drive_authority_active; then
        if declare -F emit_event >/dev/null 2>&1; then
            emit_event "hook" "artifact_edited_operator_initiated" \
                "$(jq -nc --arg fp "$FILE_PATH" --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
                    '{file_path:$fp, last_operator_edit:$ts, reason:"drive_authority_active"}' 2>/dev/null || echo '{}')" \
                2>/dev/null || true
        fi
        _approve
    fi
    _approve
    ;;
  Bash)
    # Block only when a write operator is bound to a protected path (a bare
    # mention or a read - `cat ~/.fno/graph.json` - is fine). Heuristic with a
    # known ceiling: an exotic write construct (a python one-liner opening the
    # file for write) is not caught; the merge gate's external-review artifact
    # factor is the backstop. This removes the cheap one-liner forge.
    if _bash_targets_protected "$COMMAND"; then
        _block "$_MANIFEST_REASON (this Bash write to a protected state file is blocked; use \`fno backlog\` / \`fno state\`)."
    fi
    _approve
    ;;
  *)
    _approve
    ;;
esac

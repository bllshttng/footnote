#!/usr/bin/env bash
# precompact-canon-doc.sh - PreCompact mechanical backstop.
#
# Writes and refreshes the MECHANICAL sections of this session's canon handoff
# doc (identity, live workers, node pointers, open PRs) and prints a pointer so
# compaction spends its fixed output budget on the conversation since the doc
# was written, not on re-deriving the doc's contents.
#
# What it is NOT: the writer. It cannot run a model (PreCompact supports only
# command/http/mcp_tool, not prompt/agent), so the two judgment sections -
# merge order, open decisions - are left as empty headings filled by the
# session at full context (the context-nudge asks for that). This is a
# backstop that reaches every compacting session, including the king passes and
# non-target sessions that the manifest-gated arm-handoff hook never reaches.
#
# NEVER blocks. Compaction triggered to recover from a context-limit error must
# not have its request fail at the moment it fires, so this hook always exits 0,
# never emits decision: block, and degrades to an omitted section (never a
# failure) when fno / gh / the registry is unreadable or absent.
set -uo pipefail

SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${FNO_PLATFORM:-}" == "codex" ]]; then
  PLUGIN_ROOT="${CODEX_PLUGIN_ROOT:-${PLUGIN_ROOT:-$SOURCE_ROOT}}"
else
  PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${PLUGIN_ROOT:-$SOURCE_ROOT}}"
fi
FNO_DIR=".fno"
# The session-id fallback chain lives in the shared postcompact lib so a marker
# change lands once. Unreadable lib: keep the local chain quiet, never fail.
CARRIER_LIB="$PLUGIN_ROOT/scripts/lib/postcompact-carrier.sh"
MARKER_LIB="$PLUGIN_ROOT/scripts/lib/canon-doc-marker.sh"
[[ -r "$MARKER_LIB" ]] && source "$MARKER_LIB"

# ---------------------------------------------------------------------------
# Read the hook event from stdin (non-fatal if absent).
# ---------------------------------------------------------------------------
INPUT=""
if [[ ! -t 0 ]]; then
  INPUT="$(cat 2>/dev/null || true)"
fi

_json_field() {
  # Echo a top-level string field from the hook event JSON, or "".
  printf '%s' "$INPUT" | python3 -c '
import sys, json
try:
    print(json.load(sys.stdin).get(sys.argv[1], "") or "")
except Exception:
    print("")
' "$1" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Resolve the session id through the shared postcompact lib: transcript
# basename first (PreCompact carries transcript_path), then the env markers in
# HARNESS_SESSION_MARKERS precedence. No session id -> nothing useful to point
# at; emit nothing, exit clean.
# ---------------------------------------------------------------------------
_tp="$(_json_field transcript_path)"
if [[ -r "$CARRIER_LIB" ]]; then
  # shellcheck source=../scripts/lib/postcompact-carrier.sh
  source "$CARRIER_LIB"
  SID="$(postcompact_resolve_sid "" "$_tp")"
else
  # Unreadable lib: degrade to the transcript basename only (the claude path;
  # PreCompact always carries it there) rather than duplicating the chain.
  SID=""
  if [[ -n "$_tp" ]]; then
    SID="$(basename "$_tp")"
    SID="${SID%.jsonl}"
  fi
fi
# No session id -> nothing useful to point at. Emit nothing, exit clean.
[[ -n "$SID" ]] || exit 0

# Short id for display. Labeled tail-8 because that is the mail handle today;
# the full SID above is the authoritative identity key regardless.
SHORT="${SID: -8}"

# ---------------------------------------------------------------------------
# Crowned-ness + crown scope, read before the doc-path resolution because a
# crowned session keys its doc on the SCOPE (a crown outlives its sessions; a
# successor resolves the same rolling doc). Computed once from the registry
# read and handed into the heredoc below via env: the heredoc's stdout carries
# ONLY the auto block text, matching every other fact in this script. An
# earlier version smuggled this boolean out as a synthetic first stdout line
# for bash to split off by position - fragile, since any reordering of the
# heredoc's own output silently corrupts the auto block with no error (the
# whole heredoc is wrapped in `|| true`).
# ---------------------------------------------------------------------------
REG_ROWS=""
if command -v fno >/dev/null 2>&1; then
  REG_ROWS="$(fno agents registry-json 2>/dev/null || true)"
fi

CROWN_INFO="$(SID="$SID" REG_ROWS="$REG_ROWS" python3 -c '
import json, os
sid = os.environ.get("SID", "")
try:
    data = json.loads(os.environ.get("REG_ROWS") or "[]")
except Exception:
    data = []
if isinstance(data, dict):
    data = data.get("agents") or data.get("rows") or []
rows = data if isinstance(data, list) else []
mine = [r for r in rows if r.get("session_id") == sid or r.get("harness_session_id") == sid]
r = mine[0] if mine else {}
lvl = r.get("crown_level")
scp = r.get("crown_scope")
print("1" if (mine and (lvl is not None or scp is not None)) else "0")
print(scp if isinstance(scp, str) else "")
' 2>/dev/null || true)"
IS_CROWNED="$(printf '%s' "$CROWN_INFO" | sed -n 1p)"
CROWN_SCOPE="$(printf '%s' "$CROWN_INFO" | sed -n 2p)"
[[ "$IS_CROWNED" == "1" ]] || IS_CROWNED=0

# ---------------------------------------------------------------------------
# Resolve the canon doc path. A manual /compact <path> carries a path the
# session deliberately chose - enrich THAT file rather than minting a sibling.
# Otherwise fall back to fno config paths handoff: --scope for a crowned
# session (the newest doc for that crown, else today's scope-keyed name), the
# session-keyed form otherwise. Only treat custom_instructions as a path when
# it plainly is one (ends in .md); prose instructions fall through.
# ---------------------------------------------------------------------------
DOC_PATH=""
_ci="$(_json_field custom_instructions)"
_ci="$(printf '%s' "$_ci" | tr -d '\r\n')"
_ci="${_ci#"${_ci%%[![:space:]]*}"}"  # trim leading whitespace
# Only treat custom_instructions as a doc path when it plainly IS one: a
# path-anchored .md route the session deliberately chose, or an existing file.
# Bare prose that happens to end in .md ("remember to update README.md") must
# fall through to fno config paths handoff, not become a junk filename in the cwd.
case "$_ci" in
  /*.md|./*.md|../*.md) DOC_PATH="$_ci" ;;
  *.md) [[ -f "$_ci" ]] && DOC_PATH="$_ci" ;;
esac
if [[ -z "$DOC_PATH" && -n "$CROWN_SCOPE" ]]; then
  DOC_PATH="$(fno config paths handoff --scope "$CROWN_SCOPE" 2>/dev/null || true)"
fi
if [[ -z "$DOC_PATH" ]]; then
  DOC_PATH="$(fno config paths handoff --session-id "$SID" 2>/dev/null || true)"
fi
# No resolvable doc path -> a bare discard list with no pointer is worth little.
[[ -n "$DOC_PATH" ]] || exit 0

# ---------------------------------------------------------------------------
# Gather mechanical facts. Each degrades to an empty/omitted value, never an
# error. Bounded subprocess calls only.
# ---------------------------------------------------------------------------
ISO="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"

# Node pointer + plan from the target manifest, if this session has one.
NODE=""
PLAN=""
if [[ -f "$FNO_DIR/target-state.md" ]]; then
  NODE="$(grep -m1 -E "^input:" "$FNO_DIR/target-state.md" 2>/dev/null | sed -E 's/^input:[[:space:]]*//; s/^"(.*)"$/\1/' || true)"
  PLAN="$(grep -m1 -E "^plan_path:" "$FNO_DIR/target-state.md" 2>/dev/null | sed -E 's/^plan_path:[[:space:]]*//; s/^"(.*)"$/\1/' || true)"
fi

PR_RAW=""
if command -v fno >/dev/null 2>&1; then
  # Omit the section entirely if fno is missing, there is no remote, or REST fails
  # - never a failed hook (the vertical-generalization epic takes fno past code).
  PR_RAW="$(fno do pr list --state open 2>/dev/null || true)"
fi

# ---------------------------------------------------------------------------
# Build the auto block. One python heredoc (quoted delimiter => no shell
# escaping) reads the facts from env and emits the mechanical sections. JSON
# parsing stays in python; bash never touches it.
# No apostrophes anywhere in this heredoc body: bash's paren-matching for the
# enclosing $(...) misreads an apostrophe followed later by a parenthesized
# comment/string as an unterminated quote, and the script fails `bash -n`
# with an EOF error that names an unrelated later line.
# ---------------------------------------------------------------------------
AUTO_BLOCK="$(SID="$SID" SHORT="$SHORT" NODE="$NODE" PLAN="$PLAN" \
             REG_ROWS="$REG_ROWS" PR_RAW="$PR_RAW" IS_CROWNED="$IS_CROWNED" python3 <<'PY' 2>/dev/null || true
import json
import os
import subprocess


def rows_from(raw):
    try:
        data = json.loads(raw) if raw else []
    except Exception:
        return []
    if isinstance(data, dict):
        data = data.get("agents") or data.get("rows") or []
    return data if isinstance(data, list) else []


sid = os.environ.get("SID", "")
short = os.environ.get("SHORT", "")
node = os.environ.get("NODE", "")
plan = os.environ.get("PLAN", "")
rows = rows_from(os.environ.get("REG_ROWS", ""))
mine = [r for r in rows if r.get("session_id") == sid or r.get("harness_session_id") == sid]
r = mine[0] if mine else {}

lvl = r.get("crown_level")
scp = r.get("crown_scope")
crowned = os.environ.get("IS_CROWNED") == "1"
if not mine:
    crown = "none (no registry row for this session)"
elif not crowned:
    crown = "none (uncrowned)"
else:
    crown = "level %s | scope %s" % (lvl if lvl is not None else "-", scp if scp is not None else "-")

live = [x for x in rows
        if x.get("spawned_by_session") == sid
        and str(x.get("status", "")).lower() not in ("exited", "dead")]
if not live:
    workers = "none"
else:
    workers = "\n".join(
        "- %s | %s | %s" % ((x.get("session_id") or "")[-8:], x.get("name", "-"), x.get("status", "-"))
        for x in live
    )

if node or plan:
    pointers = []
    if node:
        pointers.append("- node: %s" % node)
    if plan:
        pointers.append("- plan: %s" % plan)
    nodes = "\n".join(pointers)
else:
    nodes = "none (no target manifest - non-target session)"

out = ["## Identity (auto)",
       "- mail handle (tail-8): `%s`" % short,
       "- crown: %s" % crown,
       "",
       "## Live workers (auto)",
       workers,
       "",
       "## Node pointers (auto)",
       nodes]

pr_rows = rows_from(os.environ.get("PR_RAW", ""))
if pr_rows:
    out += ["", "## Open PRs (auto)"]
    out += ["- #%s [%s] %s (%s)" % (x.get("number", "-"), x.get("state", "-"),
                                    x.get("title", "-"), x.get("headRefName", "-"))
            for x in pr_rows]


def _epic_children(epic_id):
    """One epic's children via `epic status`, or None on any failure."""
    try:
        proc = subprocess.run(
            ["fno", "backlog", "epic", "status", epic_id, "--json"],
            capture_output=True, text=True, timeout=5, check=False,
        )
        data = json.loads(proc.stdout or "")
    except Exception:
        return None
    children = data.get("children")
    return children if isinstance(children, list) else None


def nodes_under_purview(scope):
    """The crown scope's children, id plus status, via the existing epic
    status read. A portfolio crown stores its scope as a comma-joined set of
    epics (fno.agents.crown.canonical_scope) - query each member and merge,
    so a level-2 king over more than one epic sees every member's children,
    not just the first. Bounded (5s per member) and degrade-only: a member
    that is not a queryable epic (a level-1 whole-project crown, an
    unreadable graph) contributes nothing rather than failing the whole
    read; this whole block is skipped only when EVERY member yields nothing."""
    if not scope:
        return None
    rows = []
    seen_ids = set()
    for member in (part.strip() for part in scope.split(",")):
        if not member:
            continue
        for c in _epic_children(member) or []:
            node_id = c.get("id")
            if node_id in seen_ids:
                continue
            seen_ids.add(node_id)
            rows.append(c)
    if not rows:
        return None
    return "\n".join(
        "- %s [%s] %s" % (c.get("id", "-"), c.get("status", "-"), c.get("slug", ""))
        for c in rows
    )


# A king additionally holds its nodes under purview and its own live workers -
# facts no other session has. Skipped whole when uncrowned so a non-king canon
# doc stays byte-identical to the prior output (AC2-EDGE).
if crowned:
    nodes = nodes_under_purview(scp) or "_(nodes under purview: unavailable)_"
    out += [
        "",
        "## King: nodes under purview (auto)",
        "level %s over %s" % (lvl if lvl is not None else "-", scp if scp is not None else "-"),
        "",
        nodes,
        "",
        # `workers` is spawned_by_session, not crown-scope-filtered: a
        # successor omits a predecessor workers list, and unrelated-territory
        # workers of this session leak in. Follow-up under x-5226.
        "## King: live workers in scope (auto)",
        workers,
    ]

print("\n".join(out))
PY
)"

# ---------------------------------------------------------------------------
# Preserve any judgment the session already wrote under the two session markers.
# The hook regenerates the auto block on every fire; it must NOT clobber what
# the session filled at full context (the compact that triggers this hook would
# otherwise erase the judgment right when post-compact context needs it).
# ---------------------------------------------------------------------------
_session_block() {
  # $1 = heading label substring, $2 = default instruction text. Matched by
  # the "## <label>" heading immediately above each marker, not by ordinal
  # position: a king who hand-writes only the two crown headings on a first
  # compaction (the doc did not exist yet to hold headings 1/2) still binds
  # correctly, instead of silently landing in the wrong slot.
  local label="$1" default="$2" preserved=""
  if [[ -f "$DOC_PATH" ]]; then
    preserved="$(awk -v label="$label" '
      /^## / { heading = $0; next }
      /<!-- fno:session -->/ { grab = (index(heading, label) > 0); next }
      grab && /<!-- \/fno:session -->/ { grab=0; next }
      grab { print }
    ' "$DOC_PATH" 2>/dev/null)"
  fi
  if [[ -n "$(printf '%s' "$preserved" | tr -d '[:space:]')" ]]; then
    printf '%s\n' "$preserved"
  else
    printf '%s\n' "$default"
  fi
}

DEFAULT_MERGE="_Merge order and the reason for it. Nothing external knows this. The session fills it at full context._"
DEFAULT_DECISIONS="_Open decisions awaiting the operator. Nothing external knows this. The session fills it at full context._"
DEFAULT_GAPS="_Gaps and open thinking only this crown holds. Nothing external knows this. The session fills it at full context._"
DEFAULT_WORKAROUNDS="_Workarounds in force only this crown is running. Nothing external knows this. The session fills it at full context._"

# Capture the preserved-or-defaulted session blocks BEFORE opening the doc for
# write. The assembly below redirects to $DOC_PATH, which truncates it on open;
# reading inside that block would see an empty file and always default.
SB1="$(_session_block "Merge order and why" "$DEFAULT_MERGE")"
SB2="$(_session_block "Open decisions awaiting the operator" "$DEFAULT_DECISIONS")"
SB3="" SB4=""
if [[ "$IS_CROWNED" == "1" ]]; then
  SB3="$(_session_block "Gaps and open thinking" "$DEFAULT_GAPS")"
  SB4="$(_session_block "Workarounds in force" "$DEFAULT_WORKAROUNDS")"
fi

# A doc the session wrote by hand carries none of the markers above, so the
# preserve helper reads nothing from it and the write below would truncate it
# whole - erasing a brief written at full context. Keep everything that sits
# above the hook's own title line. Re-fires re-read the same span, so the body
# is preserved once, not appended again.
PRIOR=""
if [[ -f "$DOC_PATH" ]]; then
  PRIOR="$(awk '/^# Canon doc: /{exit} {print}' "$DOC_PATH" 2>/dev/null)"
fi

# The fno:user block is the one section the machine NEVER writes and ALWAYS
# reads: whatever the user typed between its markers round-trips byte-for-byte,
# and the closing marker is re-added below when a partial edit dropped it
# (capture runs to the next heading or EOF, so nothing is lost either way).
# Captured before the truncate like the session blocks. Seeded only when no
# open marker exists yet; from then on the placeholder is ordinary content.
USER_BLOCK=""
USER_SEED=1
if [[ -f "$DOC_PATH" ]] && command -v canon_doc_extract_marker >/dev/null 2>&1; then
  USER_BLOCK="$(canon_doc_extract_marker "$DOC_PATH" user)" && USER_SEED=0
fi
if [[ "$USER_SEED" == "1" ]]; then
  USER_BLOCK="$(canon_doc_user_placeholder)"
fi

# ---------------------------------------------------------------------------
# Assemble the doc. Auto block fully regenerated; the two session blocks are
# preserved-or-defaulted. Ensure the parent dir exists (handoffs_dir may resolve
# to a path that does not yet exist, e.g. the state-dir fallback on a fresh
# setup); without this the write fails silently and the pointer below would lie.
# ---------------------------------------------------------------------------
mkdir -p "$(dirname "$DOC_PATH")" 2>/dev/null || true
{
  if [[ -n "$(printf '%s' "$PRIOR" | tr -d '[:space:]')" ]]; then
    printf '%s\n\n' "$PRIOR"
  fi
  # A crowned session's doc is the crown's rolling doc (scope-keyed name), so
  # the title names the crown; a successor reading it is not session ${SHORT}.
  if [[ "$IS_CROWNED" == "1" && -n "$CROWN_SCOPE" ]]; then
    echo "# Canon doc: crown ${CROWN_SCOPE}"
  else
    echo "# Canon doc: session ${SHORT}"
  fi
  echo ""
  echo "Session id (authoritative): \`${SID}\`  |  refreshed ${ISO} by precompact-canon-doc.sh."
  # Blank line, deliberately: a paragraph is one physical line, so two echoes
  # in a row read as a wrapped paragraph and `fno doctor lint style` refuses
  # the doc this hook just wrote.
  echo ""
  echo "Mechanical sections below are auto-generated. The two judgment sections are filled by the session."
  echo ""
  echo "<!-- fno:auto -->"
  printf '%s\n' "$AUTO_BLOCK"
  echo "<!-- /fno:auto -->"
  echo ""
  echo "## Merge order and why (session)"
  echo "<!-- fno:session -->"
  printf '%s\n' "$SB1"
  echo "<!-- /fno:session -->"
  echo ""
  echo "## Open decisions awaiting the operator (session)"
  echo "<!-- fno:session -->"
  printf '%s\n' "$SB2"
  echo "<!-- /fno:session -->"
  echo ""
  if [[ "$IS_CROWNED" == "1" ]]; then
    echo "## Gaps and open thinking (session)"
    echo "<!-- fno:session -->"
    printf '%s\n' "$SB3"
    echo "<!-- /fno:session -->"
    echo ""
    echo "## Workarounds in force (session)"
    echo "<!-- fno:session -->"
    printf '%s\n' "$SB4"
    echo "<!-- /fno:session -->"
    echo ""
  fi
  echo "## User notes (you write here; the machine only ever reads this)"
  echo "<!-- fno:user -->"
  printf '%s\n' "$USER_BLOCK"
  echo "<!-- /fno:user -->"
} > "$DOC_PATH" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Stdout: spend the budget on DISCARD, not preserve. The base compaction prompt
# already demands the nine preservation categories; restating them competes for
# the same fixed 20k-token budget. Point at the doc and name what to drop.
# ---------------------------------------------------------------------------
PLAN_LINE=""
if [[ -n "$PLAN" ]]; then
  PLAN_LINE="The plan at ${PLAN} describes intent, not what happened."
fi
cat <<EOF
A canon doc for this session exists at ${DOC_PATH} and is authoritative for
decisions, open questions, and worker state. Do not re-derive its contents
in the summary; spend the budget on the conversation since it was written.
${PLAN_LINE}
Discard raw grep/glob output, files read only for exploration, and
intermediate debugging output - keep conclusions only.
EOF

exit 0

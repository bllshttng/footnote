#!/usr/bin/env bash
# graph-resolve.sh -- resolve "ab-xxxxxxxx" graph IDs to plan_path.
#
# Source this file, then call resolve_arg with any user-supplied argument.
# Behaviors:
#   - Full ab-XXXXXXXX  -> exact match, echoes plan_path or soft-fails.
#   - Partial ab-XXXX..XXXXXXX (4-7 hex chars) -> prefix match via
#       fno.graph.fuzzy.resolve_id; echoes plan_path on a unique
#       resolution, soft-fails (with stderr) on ambiguity / no match.
#   - RESOLVE_FUZZY=1 + non-ab input -> title fuzzy match via resolve_id.
#       Off by default because /target etc. pass raw feature descriptions
#       that must NOT be collapsed onto an existing graph node.
#   - Anything else -> echoes arg unchanged.
#
# Usage:
#   source scripts/lib/graph-resolve.sh
#   arg=$(resolve_arg "$1")
#
# Env:
#   GRAPH_JSON        override graph.json path (default: ~/.fno/graph.json)
#   RESOLVE_STRICT=1  exit nonzero on unknown / ambiguous queries
#   RESOLVE_FUZZY=1   opt into title fuzzy match for non-ab queries
#
# Design notes:
# - Env-passed inputs to the python heredoc avoid shell-quoting hell (plan
#   paths with spaces, unicode titles). No `-c "$arg"` interpolation means no
#   injection surface: the arg is bound to os.environ, never spliced into a
#   shell or python string.
# - The python module path requires the `fno` package to be importable.
#   When the import fails (e.g. environments without uv / venv), the resolver
#   soft-fails to echoing the arg unchanged, preserving the historical
#   contract for non-Python environments.
# - Soft fail by default. Downstream skills then try the echoed value as a
#   file path, which fails with a clearer error than a bash function dying
#   silently. RESOLVE_STRICT=1 opts into hard fail.

# Use GRAPH_JSON_PATH from paths.sh if available; fall back to hardcoded default.
if [[ -z "${GRAPH_JSON_PATH:-}" ]] && command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
GRAPH_JSON="${GRAPH_JSON:-${GRAPH_JSON_PATH:-$HOME/.fno/graph.json}}"

resolve_arg() {
    local arg="$1"
    # Pass through anything that isn't an ab- query unless the caller
    # explicitly opts in to title fuzzy matching. Most /target callers pass
    # raw feature descriptions that we must not collapse to a graph node.
    if [[ ! "$arg" =~ ^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$ ]] && [[ "${RESOLVE_FUZZY:-0}" != "1" ]]; then
        echo "$arg"
        return 0
    fi
    if [[ ! -f "$GRAPH_JSON" ]]; then
        echo "[graph-resolve] $GRAPH_JSON missing; using '$arg' as-is" >&2
        echo "$arg"
        return 0
    fi

    local result rc
    result=$(GRAPH_JSON="$GRAPH_JSON" QUERY="$arg" python3 - <<'PYEOF'
import json, os, sys
try:
    from fno.graph.fuzzy import resolve_id
except ImportError as e:
    sys.stderr.write(f"[graph-resolve] fno.graph.fuzzy import failed: {e}\n")
    sys.exit(5)
graph_path = os.environ["GRAPH_JSON"]
query = os.environ["QUERY"]
try:
    with open(graph_path) as f:
        data = json.load(f)
except Exception as e:
    sys.stderr.write(f"[graph-resolve] failed to read {graph_path}: {e}\n")
    sys.exit(2)
entries = data.get("entries", [])
match = resolve_id(query, entries)
if match.kind in ("exact", "fuzzy") and match.candidates:
    # candidates[0] is the matched entry; resolve_id already did the
    # iteration so we don't repeat it here.
    matched = match.candidates[0]
    if matched.get("plan_path"):
        sys.stdout.write(matched["plan_path"])
        sys.exit(0)
    sys.exit(3)  # resolved id but no plan_path
if match.kind == "ambiguous":
    candidate_ids = ", ".join(e.get("id", "?") for e in match.candidates)
    sys.stderr.write(f"[graph-resolve] ambiguous '{query}' matches: {candidate_ids}\n")
    sys.exit(4)
sys.stderr.write(f"[graph-resolve] no match for '{query}'\n")
sys.exit(1)
PYEOF
)
    rc=$?
    case $rc in
        0) echo "$result" ;;
        1)
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
        3)
            echo "[graph-resolve] node '$arg' has no plan_path in $GRAPH_JSON" >&2
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
        4)
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
        5)
            # Python module not importable; fall back to legacy exact-match
            # path so non-Python environments keep the old behavior.
            # Print one explicit notice so the user understands the partial-
            # prefix path will be unavailable in this environment - the
            # import-error stderr from the heredoc on its own can read like
            # a fatal failure when the resolver actually succeeded with the
            # legacy matcher.
            echo "[graph-resolve] fno package unavailable; falling back to legacy exact-match resolver (partial-prefix queries will not resolve)" >&2
            _resolve_arg_legacy "$arg"
            return $?
            ;;
        *)
            echo "[graph-resolve] lookup failed (rc=$rc) for '$arg'" >&2
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
    esac
    return 0
}

# Legacy fallback: exact-match only, no prefix support. Used when the
# fno package can't be imported (no uv, no venv, no PYTHONPATH).
# Mirrors the pre-fuzzy behavior so older environments are not regressed.
_resolve_arg_legacy() {
    local arg="$1"
    if [[ ! "$arg" =~ ^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$ ]]; then
        # A partial/short id (fewer hex than a real id) cannot be resolved
        # without the python module. Surface this explicitly so a user typing a
        # prefix in a non-Python environment doesn't see silent passthrough
        # and assume the resolver worked.
        if [[ "$arg" =~ ^[a-z][a-z0-9]{0,7}-[0-9a-f]+$ ]]; then
            echo "[graph-resolve] partial/short node id '$arg' cannot resolve in legacy mode; pass a full <prefix>-<4..8 hex> id or install the fno python package" >&2
        fi
        echo "$arg"
        return 0
    fi
    local path rc
    path=$(GRAPH_JSON="$GRAPH_JSON" TARGET="$arg" python3 - <<'PYEOF'
import json, os, sys
path = os.environ["GRAPH_JSON"]
target = os.environ["TARGET"]
try:
    with open(path) as f:
        data = json.load(f)
except Exception as e:
    sys.stderr.write(f"[graph-resolve] failed to read {path}: {e}\n")
    sys.exit(2)
for entry in data.get("entries", []):
    if entry.get("id") == target:
        plan_path = entry.get("plan_path") or ""
        sys.stdout.write(plan_path)
        sys.exit(0 if plan_path else 3)
sys.exit(1)
PYEOF
)
    rc=$?
    case $rc in
        0) echo "$path" ;;
        1)
            echo "[graph-resolve] unknown id '$arg' in $GRAPH_JSON" >&2
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
        3)
            echo "[graph-resolve] node '$arg' has no plan_path in $GRAPH_JSON" >&2
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
        *)
            echo "[graph-resolve] legacy lookup failed (rc=$rc) for '$arg'" >&2
            [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
            echo "$arg"
            ;;
    esac
    return 0
}

#!/usr/bin/env bash
# graph-resolve.sh -- resolve "ab-xxxxxxxx" graph IDs to plan_path.
#
# Source this file, then call resolve_arg with any user-supplied argument.
# Behaviors:
#   - Full ab-XXXXXXXX  -> exact match via `fno backlog get`; echoes plan_path
#       or soft-fails.
#   - Partial ab-XXXX..XXXXXXX (4-7 hex chars) -> prefix match through the
#       same verb; echoes plan_path on a unique resolution, soft-fails (with
#       stderr) on ambiguity / no match.
#   - RESOLVE_FUZZY=1 + non-ab input -> title fuzzy match through the verb.
#       Off by default because /target etc. pass raw feature descriptions
#       that must NOT be collapsed onto an existing graph node.
#   - Anything else -> echoes arg unchanged.
#
# Usage:
#   source scripts/lib/graph-resolve.sh
#   arg=$(resolve_arg "$1")
#
# Env:
#   RESOLVE_STRICT=1  exit nonzero on unknown / ambiguous queries
#   RESOLVE_FUZZY=1   opt into title fuzzy match for non-ab queries
#
# Design notes:
# - `fno backlog get` is the resolution seam: it owns the id/slug/bare-hex/
#   fuzzy tiers and the tracker-backend switch, so this shim never opens the
#   graph store itself. Store path overrides ride fno's own path config.
# - Soft fail by default. Downstream skills then try the echoed value as a
#   file path, which fails with a clearer error than a bash function dying
#   silently. RESOLVE_STRICT=1 opts into hard fail.

# Single fno-vs-external-vs-none classifier, shared with parse-claims-arg.sh so
# the id-shape test has one home. Sourced as a bundled sibling (BASH_SOURCE
# resolves beside this file at both the repo-root and skills/ locations).
source "$(dirname "${BASH_SOURCE[0]}")/node-id.sh"

resolve_arg() {
    local arg="$1"
    local kind
    kind="$(node_id_kind "$arg")"
    # External ids are opaque work handles, never graph-resolvable. Not even
    # under RESOLVE_FUZZY, which is for title matching rather than id matching.
    if [[ "$kind" == "external" ]]; then
        echo "$arg"
        return 0
    fi
    # Non-ids pass through unless the caller opted into title fuzzy matching.
    # Most /target callers pass raw feature descriptions that we must not
    # collapse to a graph node.
    if [[ "$kind" == "none" ]] && [[ "${RESOLVE_FUZZY:-0}" != "1" ]]; then
        echo "$arg"
        return 0
    fi
    # A GRAPH_JSON override is a sandbox contract (the shim's own tests, and
    # callers pinning a scratch graph): resolve against that file. Otherwise
    # resolve against the ambient store through the configured path,
    # backend-switched like every other reader. Both ride the plugin's own
    # cli/src, so the full resolver tiers (exact id, unique prefix, opt-in
    # title fuzzy) survive whether this copy runs from a repo checkout or a
    # deployed plugin directory. Exit contract:
    #   0 plan_path | 1 no match | 3 no plan_path | 4 ambiguous
    #   5 package unimportable -> the `fno backlog get` fallback below
    #   6 external tracker backend -> pass the arg through unchanged
    local plugin_root="${FNO_RESOLVE_PLUGIN_ROOT:-}"
    if [[ -z "$plugin_root" ]]; then
        plugin_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    fi
    local sandbox_result rc
    sandbox_result=$(PLUGIN_ROOT="$plugin_root" QUERY="$arg" python3 - <<'PYEOF' 2>/dev/null
import os, sys
sys.path.insert(0, os.path.join(os.environ["PLUGIN_ROOT"], "cli", "src"))
try:
    from fno.graph.fuzzy import resolve_id
    from fno.graph.store import read_graph_strict
except ImportError:
    sys.exit(5)
graph = os.environ.get("GRAPH_JSON") or ""
if not graph:
    try:
        from fno.paths import graph_json as configured_graph
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            sys.exit(6)
        graph = str(configured_graph())
    except Exception:
        sys.exit(6)
from pathlib import Path
try:
    entries = read_graph_strict(Path(graph))
except Exception:
    sys.exit(1)
match = resolve_id(os.environ["QUERY"], entries)
if match.kind in ("exact", "fuzzy", "branch_derived") and match.candidates:
    matched = match.candidates[0]
    if matched.get("plan_path"):
        sys.stdout.write(matched["plan_path"])
        sys.exit(0)
    sys.exit(3)
if match.kind == "ambiguous":
    sys.exit(4)
sys.exit(1)
PYEOF
)
    rc=$?
    [[ $rc -ne 0 ]] && sandbox_result=""
    if [[ $rc -eq 0 && -n "$sandbox_result" ]]; then
        echo "$sandbox_result"
        return 0
    fi
    if [[ $rc -eq 6 ]]; then
        echo "$arg"
        return 0
    fi
    if [[ $rc -eq 5 ]]; then
        : # fall through to the `fno backlog get` fallback below
    elif [[ $rc -eq 1 ]]; then
        echo "[graph-resolve] no match for '$arg'" >&2
    elif [[ $rc -eq 3 ]]; then
        echo "[graph-resolve] node '$arg' has no plan_path" >&2
    elif [[ $rc -eq 4 ]]; then
        echo "[graph-resolve] ambiguous '$arg'" >&2
    fi
    if [[ $rc -ne 5 ]]; then
        [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
        echo "$arg"
        return 0
    fi

    if ! command -v fno >/dev/null 2>&1; then
        echo "[graph-resolve] fno CLI unavailable; using '$arg' as-is" >&2
        [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
        echo "$arg"
        return 0
    fi

    local payload rc plan_path
    payload=$(fno backlog get "$arg" 2>/dev/null)
    rc=$?
    if [[ $rc -ne 0 ]]; then
        if [[ $rc -eq 1 ]]; then
            echo "[graph-resolve] no match for '$arg'" >&2
        else
            echo "[graph-resolve] lookup failed (rc=$rc) for '$arg'" >&2
        fi
        [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
        echo "$arg"
        return 0
    fi
    plan_path=$(printf '%s' "$payload" | python3 -c 'import json,sys; sys.stdout.write(json.load(sys.stdin).get("plan_path") or "")' 2>/dev/null)
    if [[ -z "$plan_path" ]]; then
        echo "[graph-resolve] node '$arg' has no plan_path" >&2
        [[ "${RESOLVE_STRICT:-}" == "1" ]] && return 1
        echo "$arg"
        return 0
    fi
    echo "$plan_path"
}

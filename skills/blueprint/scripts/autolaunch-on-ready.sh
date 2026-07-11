#!/usr/bin/env bash
# autolaunch-on-ready.sh - Task 3.2 (US6): opt-in, ready-gated auto-launch on
# /blueprint completion. DEFAULT OFF and back-compatible (absent config => off,
# so Phase-1 behavior is unchanged for everyone who has not opted in).
#
# After /blueprint writes a plan and intakes its claimed backlog node, this
# script decides whether to fire-and-forget dispatch that node as a bg /target
# worker (reusing skills/target/scripts/dispatch-node.sh). It NEVER launches a
# blocked/deferred node (pre-planned future work is parked), defaults to
# no-merge (an auto-fired full pipeline lands a PR for review, not an
# auto-merge - Locked Decision 4), and on any dispatch failure leaves the node
# ready and the blueprinted plan intact for a manual `/target bg <node>` retry.
#
# Usage: autolaunch-on-ready.sh <plan-path> [--dry-run]
#
# Output (exactly one decision line when the gate is ON; never silent):
#   auto-launched   <node> name=target-<id>-<slug> session=<sid> hint="fno agents logs ..."
#   parked          <node> reason="<status> (not up-next)"
#   already-running <node> reason="..."
#   autolaunch-failed <node> reason="..."
# When the gate is OFF, or no backlog node can be resolved from the plan: silent exit 0.

set -uo pipefail

PLAN_PATH="${1:-}"
DRY=""
[[ "${2:-}" == "--dry-run" ]] && DRY="--dry-run"
if [[ -z "$PLAN_PATH" || ! -f "$PLAN_PATH" ]]; then
  echo "autolaunch: plan not found: ${PLAN_PATH:-<none>}" >&2
  exit 0
fi

# Resolve the repo root from THIS SCRIPT's location, not the plan's. A plan is
# frequently reached via the `internal/` symlink (which points at the Obsidian
# vault, outside the repo), so `git -C <plan-dir>` would resolve the wrong repo
# (or none). The script itself always lives at skills/blueprint/scripts/ inside
# the worktree, so it is the reliable anchor for finding scripts/lib/config.sh
# and skills/target/scripts/dispatch-node.sh.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || (cd "$SCRIPT_DIR/../../.." && pwd))}"

# 1. Gate: opt-in, default OFF, back-compatible (no key => off). Mirrors the
#    config.target.dedupe_dead_duplicates pattern (stale-owner.sh). Source
#    config.sh ONLY if get_config is not already defined, so a caller or test
#    can pre-define/stub it (via `export -f get_config`) for hermetic gate
#    control without depending on yq for the dotted key.
if ! declare -F get_config >/dev/null 2>&1 && [[ -f "$REPO_ROOT/scripts/lib/config.sh" ]]; then
  # shellcheck disable=SC1091
  source "$REPO_ROOT/scripts/lib/config.sh" 2>/dev/null || true
fi
enabled="false"
if declare -F get_config >/dev/null 2>&1; then
  enabled=$(get_config "target.auto_launch_on_blueprint" "false" 2>/dev/null || echo "false")
fi
case "$enabled" in true|True|TRUE|1|yes|on) ;; *) exit 0 ;; esac

# 2. Resolve the backlog node from the plan. Three tiers, first hit wins, so
#    every plan shape /blueprint produces can auto-launch (bug ab-6f93f87a):
#      a. `claims: ab-XXXXXXXX`        - quick/full plan claiming an existing node
#      b. `graph_node_id: ab-XXXXXXXX` - lean single-doc blueprint (the default;
#                                        carries the /think doc's node link and
#                                        never writes claims:)
#      c. plan_path match on the graph - a fresh-intake plan whose new node id
#                                        lives only on the graph, keyed by the
#                                        plan_path that `fno backlog intake` stored
#                                        (no frontmatter link at all)
#
# Tiers a/b read ONLY the YAML frontmatter block, never the whole file: a design
# doc body or fenced example often contains a line like `graph_node_id: ab-...`
# as prose, and a whole-file grep would treat that as authoritative and dispatch
# a stale/unrelated node instead of falling through to the plan_path lookup
# (codex P2 on PR #492).
_FRONTMATTER="$(awk '
  NR==1 && $0 !~ /^---[[:space:]]*$/ { exit }   # no frontmatter block at all
  /^---[[:space:]]*$/ { fence++; if (fence==2) exit; next }
  fence==1 { print }
' "$PLAN_PATH" 2>/dev/null)"
# The accepted id shape is the config-free NODE_ID_BODY liberal format check
# (fno.graph._constants: <prefix><hex>, prefix a 1-8 char letter-led token, hex
# 4-8 wide). It matches a configured `x-8af8` and the legacy `ab-<8hex>` alike -
# and, unlike a single config-derived prefix, still accepts legacy `ab-` nodes in
# a repo mid-migration. Hard-coding `ab-<8hex>` silently never launched `x-` nodes.
_NODE_ID_RE='[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}'
node="$(printf '%s\n' "$_FRONTMATTER" \
        | grep -m1 -E "^claims:[[:space:]]+${_NODE_ID_RE}" \
        | sed -E 's/^claims:[[:space:]]+//; s/[[:space:]].*$//; s/\r$//')"
if [[ -z "$node" ]]; then
  node="$(printf '%s\n' "$_FRONTMATTER" \
          | grep -m1 -E "^graph_node_id:[[:space:]]+${_NODE_ID_RE}" \
          | sed -E 's/^graph_node_id:[[:space:]]+//; s/[[:space:]].*$//; s/\r$//')"
fi
unset _FRONTMATTER
if [[ -z "$node" ]] && command -v python3 >/dev/null 2>&1; then
  # Tier c: best-effort plan_path lookup against graph.json. Resolve the graph
  # path the same way scripts/lib/graph-resolve.sh does (env -> fno paths
  # shell-stub -> ~/.fno/graph.json). NEVER fails the gate: any miss leaves
  # $node empty and falls through to the "nothing to launch" exit below.
  if [[ -z "${GRAPH_JSON:-}" ]] && command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    if [[ -n "$_PATHS_SH" && -f "$_PATHS_SH" ]]; then
      # shellcheck disable=SC1090
      source "$_PATHS_SH" 2>/dev/null || true
    fi
    unset _PATHS_SH
  fi
  _GRAPH_JSON="${GRAPH_JSON:-${GRAPH_JSON_PATH:-$HOME/.fno/graph.json}}"
  if [[ -f "$_GRAPH_JSON" ]]; then
    node="$(GRAPH_JSON="$_GRAPH_JSON" PLAN="$PLAN_PATH" REPO_ROOT="$REPO_ROOT" python3 - <<'PYEOF'
import json, os, sys
graph = os.environ["GRAPH_JSON"]
plan = os.environ["PLAN"]
root = os.environ.get("REPO_ROOT", "")
def norm(p):
    if not p:
        return ""
    cand = p if os.path.isabs(p) else os.path.join(root, p)
    try:
        return os.path.realpath(cand)
    except OSError:
        return os.path.abspath(cand)
try:
    with open(graph) as f:
        data = json.load(f)
except (OSError, ValueError):
    sys.exit(0)  # unreadable / malformed graph -> no resolution, gate parks safely
# Normally {"entries": [...]}, but tolerate a bare top-level list and skip any
# non-dict entry rather than crashing on .get (gemini medium, PR #492).
if isinstance(data, dict):
    entries = data.get("entries", [])
elif isinstance(data, list):
    entries = data
else:
    entries = []
want = norm(plan)
ready = other = ""
for e in entries:
    if not isinstance(e, dict):
        continue
    pp = e.get("plan_path")
    if not pp:
        continue
    if pp == plan or norm(pp) == want:
        nid = e.get("id") or ""
        if e.get("_status") == "ready":
            ready = nid
            break
        other = other or nid
print(ready or other)
PYEOF
)"
    node="$(printf '%s' "$node" | tr -d '[:space:]')"
  fi
fi
if [[ -z "$node" ]]; then
  echo "autolaunch: gate ON but $PLAN_PATH declares no resolvable backlog node (no claims:/graph_node_id:, no plan_path match on the graph); nothing to launch" >&2
  exit 0
fi

# 2b. Epic redirect (the bug this script fixes): if the resolved node was
#     decomposed - i.e. other graph nodes carry parent==node - NEVER launch the
#     epic itself. An epic-level /target builds every child's waves into one giant
#     PR, undoing the split that just happened (this is the same reason `backlog
#     next` excludes epics). Launch the first ready child instead; if children
#     exist but none is ready yet (all claimed/blocked), park - the ready one
#     autolaunches when its deps clear.
#     ponytail: "first ready child" = graph order; _status==ready already encodes
#     deps-satisfied, so no separate blocked_by walk. Graph-path resolution mirrors
#     the tier-c block above; unreadable/absent graph -> no redirect (dispatch as-is).
if command -v python3 >/dev/null 2>&1; then
  if [[ -z "${GRAPH_JSON:-}" ]] && command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    if [[ -n "$_PATHS_SH" && -f "$_PATHS_SH" ]]; then
      # shellcheck disable=SC1090
      source "$_PATHS_SH" 2>/dev/null || true
    fi
    unset _PATHS_SH
  fi
  _GRAPH_JSON="${GRAPH_JSON:-${GRAPH_JSON_PATH:-$HOME/.fno/graph.json}}"
  if [[ -f "$_GRAPH_JSON" ]]; then
    _child="$(GRAPH_JSON="$_GRAPH_JSON" NODE="$node" python3 - <<'PYEOF'
import json, os, sys
try:
    with open(os.environ["GRAPH_JSON"]) as f:
        data = json.load(f)
except (OSError, ValueError):
    sys.exit(0)  # unreadable/malformed graph -> no redirect, dispatch node as-is
entries = data.get("entries", []) if isinstance(data, dict) else (data if isinstance(data, list) else [])
node = os.environ["NODE"]
children = [e for e in entries if isinstance(e, dict) and e.get("parent") == node]
if not children:
    sys.exit(0)  # not an epic (no children) -> empty output, no redirect
for e in children:
    if e.get("_status") == "ready":
        print(e.get("id") or "")
        sys.exit(0)
print("__NONE__")  # decomposed but no ready child yet
PYEOF
)"
    _child="$(printf '%s' "$_child" | tr -d '[:space:]')"
    if [[ "$_child" == "__NONE__" ]]; then
      echo "parked $node reason=\"epic-decomposed-no-ready-child\""
      exit 0
    elif [[ -n "$_child" ]]; then
      _epic_redirect="$node"
      node="$_child"
    fi
  fi
fi

# 3. Ready-gate (Locked Decision 3): only `ready` nodes are up-next. A node that
#    is blocked/deferred/idea is parked as pre-planned future work. (Need jq +
#    fno; if either is missing we cannot gate safely, so we park, never launch.)
if ! command -v fno >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
  echo "parked $node reason=\"cannot resolve status (fno/jq missing) - not launched\""
  exit 0
fi
# Capture the read exit code so a TRANSIENT `fno backlog get` failure is NOT
# mislabeled as a not-ready status (which would silently park a genuinely-ready
# node and lie about why). A read failure parks with an honest reason; only a
# successfully-read non-`ready` status is reported as future-work.
node_json="$(fno backlog get "$node" 2>/dev/null)"; get_rc=$?
if [[ "$get_rc" -ne 0 || -z "$node_json" ]]; then
  echo "parked $node reason=\"backlog status read failed (rc=$get_rc) - not launched\""
  exit 0
fi
status="$(printf '%s' "$node_json" | jq -r '._status // "unknown"' 2>/dev/null || echo "unknown")"
if [[ "$status" != "ready" ]]; then
  echo "parked $node reason=\"$status (not up-next) - blueprinted, awaiting deps/undefer\""
  exit 0
fi

# 4. Caller-is-holder gate (absorbs cv-60186ad3, plan sec 2.4).
#    If the invoking session's target-state.md has graph_node_id == this node,
#    we ARE the live worker on this node. Blind-spawning a second worker would
#    violate the one-worker-per-node invariant. Park instead and let the LLM
#    invoke handoff.sh explicitly at the blueprint->do boundary.
#
#    Degrade safely: if the manifest is unreadable for any reason, fall through
#    to today's blind-spawn behavior (absence of evidence of holding != holding).
_MANIFEST="${REPO_ROOT}/.fno/target-state.md"
if [[ -f "$_MANIFEST" ]]; then
  _MANIFEST_NODE="$(awk '
    BEGIN{fm=0;count=0}
    /^---/{count++;if(count==2){fm=0;next}else{fm=1;next}}
    !fm && /^graph_node_id:/{print;exit}
  ' "$_MANIFEST" 2>/dev/null \
    | sed "s/^graph_node_id:[[:space:]]*//" | tr -d '"' | tr -d "'" \
    | tr -d ' ' || true)"
  if [[ -n "$_MANIFEST_NODE" && "$_MANIFEST_NODE" == "$node" ]]; then
    echo "parked $node reason=\"caller-is-holder: structural handoff at blueprint->do is the sanctioned path (skills/target/scripts/handoff.sh); not blind-spawning\""
    exit 0
  fi
fi

# 5. Dispatch via the target dispatch primitive (no-merge default; the script
#    injects no-merge unless --allow-merge). Surface the outcome; on failure the
#    node stays ready and the plan is intact (AC6-FR).
DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"
if [[ ! -f "$DISPATCH" ]]; then
  echo "autolaunch-failed $node reason=\"dispatch primitive missing at $DISPATCH\""
  exit 0
fi

out="$(bash "$DISPATCH" ${DRY:+$DRY} "$node" 2>&1)"
line="$(printf '%s\n' "$out" | grep -E '^(launched|already-running|failed|parked|skipped-done) ' | head -1)"
case "$line" in
  launched\ *)        if [[ -n "${_epic_redirect:-}" ]]; then
                        echo "auto-${line} (first ready child of epic ${_epic_redirect})"
                      else
                        echo "auto-${line}"                # -> "auto-launched <node> ..."
                      fi ;;
  already-running\ *) echo "$line" ;;
  parked\ *)          echo "$line" ;;
  skipped-done\ *)    echo "$line" ;;
  failed\ *)          echo "autolaunch-${line}" ;;      # -> "autolaunch-failed <node> ..."
  *)                  echo "autolaunch-failed $node reason=\"unexpected dispatch output: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)\"" ;;
esac
exit 0

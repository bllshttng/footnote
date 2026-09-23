#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: settled-pr-watch.sh <scope> <king-session-id> [interval-seconds]" >&2
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
scope="$1"
king_session="$2"
interval="${3:-600}"
[[ "$interval" =~ ^[0-9]+$ ]] || usage

state_path="$(fno-agents state path 2>/dev/null || true)"
if [[ -n "${FNO_REIGN_WATCH_DIR:-}" ]]; then
  watch_dir="$FNO_REIGN_WATCH_DIR"
elif [[ -n "$state_path" ]]; then
  watch_dir="$(dirname "$state_path")/reign-watch"
else
  watch_dir="${HOME:-/tmp}/.fno/reign-watch"
fi
mkdir -p "$watch_dir"

scope_key="$(printf '%s' "$scope" | tr -c 'A-Za-z0-9_.-' '_')"
lock_dir="$watch_dir/$scope_key.pid.lock"
if ! mkdir "$lock_dir" 2>/dev/null; then
  live_pid="$(cat "$lock_dir/pid" 2>/dev/null || true)"
  if [[ "$live_pid" =~ ^[0-9]+$ ]] && kill -0 "$live_pid" 2>/dev/null; then
    printf 'watch live pid %s\n' "$live_pid"
    exit 0
  fi
  rm -rf "$lock_dir"
  mkdir "$lock_dir"
fi
printf '%s\n' "$$" > "$lock_dir/pid"
cleanup() {
  if [[ "$(cat "$lock_dir/pid" 2>/dev/null || true)" == "$$" ]]; then
    rm -rf "$lock_dir"
  fi
}
trap cleanup EXIT INT TERM

probe_failed() {
  local reading="$1"
  fno agents mail send --to-king "$scope" --from-name reign-watch \
    "reign watch probe failed: $reading" >/dev/null 2>&1 || true
  printf 'reign watch probe failed: %s\n' "$reading" >&2
  exit 1
}

owned_nodes() {
  python3 -c '
import json
import sys

session = sys.argv[1]
payload = json.load(sys.stdin)
crowns = payload.get("crowns") if isinstance(payload, dict) else payload
if not isinstance(crowns, list):
    raise ValueError("court payload has no crowns list")
for crown in crowns:
    if not isinstance(crown, dict) or crown.get("manifest_session") != session:
        continue
    fold = crown.get("scope_nodes")
    if not isinstance(fold, dict) or fold.get("status") == "unresolved":
        raise ValueError("court scope fold is unreadable")
    nodes = fold.get("nodes")
    if nodes is None:
        raise ValueError("court scope fold has no nodes")
    for node in nodes:
        if not isinstance(node, dict) or node.get("owned") is not True:
            continue
        node_id = node.get("id")
        pr = node.get("pr_number") or node.get("pr")
        if isinstance(node_id, str) and isinstance(pr, int) and pr > 0:
            print(f"{node_id}\t{pr}")
    break
else:
    print("NO_COURT_ROW", file=sys.stderr)
    raise SystemExit(2)
' "$king_session"
}

event_matches() {
  python3 -c '
import json
import sys

payload = json.load(sys.stdin)
if not isinstance(payload, dict):
    raise ValueError("event find payload is not an object")
unreadable = payload.get("unreadable_files")
if not isinstance(unreadable, list):
    raise ValueError("event find payload has no unreadable_files list")
if unreadable:
    print(f"unreadable_files={len(unreadable)}", file=sys.stderr)
    raise SystemExit(3)
for match in payload.get("matches", []):
    if not isinstance(match, dict):
        continue
    data = match.get("data") if isinstance(match.get("data"), dict) else match
    node = data.get("node") or data.get("node_id")
    pr = data.get("pr") or data.get("pr_number")
    if isinstance(node, str):
        value = str(pr) if isinstance(pr, int) else ""
        print(f"{node}\t{value}")
' 
}

pr_is_ready() {
  python3 -c '
import json
import sys

payload = json.load(sys.stdin)
if not isinstance(payload, dict) or payload.get("ready") is not True:
    raise SystemExit(1)
blockers = payload.get("ready_blockers")
if blockers is None:
    blockers = payload.get("blockers")
if blockers is None and isinstance(payload.get("merge_decision"), dict):
    blockers = payload["merge_decision"].get("blockers")
if blockers not in (None, [], {}, ""):
    raise SystemExit(1)
'
}

while :; do
  court_json="$(fno agents court --nodes --json 2>/dev/null)" || probe_failed "court read failed"
  set +e
  nodes="$(owned_nodes <<<"$court_json" 2>&1)"
  court_status=$?
  set -e
  case "$court_status" in
    2) probe_failed "court row for manifest session $king_session not found" ;;
    0) ;;
    *) probe_failed "court payload unreadable: $nodes" ;;
  esac

  events_json="$(fno doctor event find pr_nudge_escalated --since 15m --json 2>/dev/null)" || probe_failed "event find failed"
  set +e
  matches="$(event_matches <<<"$events_json" 2>&1)"
  event_status=$?
  set -e
  [[ "$event_status" == 0 ]] || probe_failed "$matches"

  while IFS=$'\t' read -r node court_pr; do
    [[ -n "$node" ]] || continue
    event_pr=""
    while IFS=$'\t' read -r event_node candidate_pr; do
      [[ "$event_node" == "$node" ]] || continue
      event_pr="$candidate_pr"
      break
    done <<<"$matches"
    [[ -n "$event_pr" ]] || continue
    pr="$event_pr"
    [[ "$pr" =~ ^[0-9]+$ ]] || pr="$court_pr"
    [[ "$pr" =~ ^[0-9]+$ ]] || probe_failed "event match for $node has no PR number"
    pr_json="$(fno do pr status "$pr" 2>/dev/null)" || probe_failed "PR status #$pr failed"
    if pr_is_ready <<<"$pr_json"; then
      message="PR #$pr on $node is ready with no blockers; take the merge lever."
      fno agents mail send --to-king "$scope" --from-name reign-watch "$message" \
        >/dev/null 2>&1 || probe_failed "mail to crown failed for PR #$pr"
      printf 'watch matched PR #%s on %s\n' "$pr" "$node"
      exit 0
    fi
  done <<<"$nodes"

  [[ "$interval" == 0 ]] && exit 0
  sleep "$interval"
done

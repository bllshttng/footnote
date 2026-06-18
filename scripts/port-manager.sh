#!/usr/bin/env bash
# Port management for speculative worktrees
# Usage:
#   port-manager.sh find <count> [--start <port>]
#   port-manager.sh kill-all [state-file]
#   port-manager.sh status [state-file]
set -uo pipefail

case "${1:-status}" in
    find)
        COUNT="${2:-1}"
        START=3001
        shift 2 2>/dev/null || true
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --start) START="$2"; shift 2 ;;
                *) shift ;;
            esac
        done

        PORT=$START
        FOUND=0
        while [[ $FOUND -lt $COUNT ]]; do
            if ! lsof -i ":$PORT" >/dev/null 2>&1; then
                printf "%d " "$PORT"
                FOUND=$((FOUND + 1))
            fi
            PORT=$((PORT + 1))
            [[ $PORT -gt 65535 ]] && break
        done
        echo
        ;;

    kill-all)
        STATE_FILE="${2:-.fno/speculate-state.json}"
        if [[ ! -f "$STATE_FILE" ]]; then
            echo "No state file found"
            exit 0
        fi
        python3 -c "
import json, subprocess, sys
with open(sys.argv[1]) as f:
    state = json.load(f)
port = state.get('port_start', 3001)
killed = 0
for _ in state.get('variations', {}):
    result = subprocess.run(['lsof', '-ti', f':{port}'], capture_output=True, text=True)
    for pid in result.stdout.strip().split('\n'):
        if pid.strip():
            subprocess.run(['kill', pid.strip()], capture_output=True)
            killed += 1
    port += 1
print(f'Killed {killed} processes across {len(state.get(\"variations\", {}))} ports')
" "$STATE_FILE" 2>/dev/null
        ;;

    status)
        STATE_FILE="${2:-.fno/speculate-state.json}"
        if [[ ! -f "$STATE_FILE" ]]; then
            echo "No active speculation"
            exit 0
        fi
        python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    state = json.load(f)
print(f\"Feature: {state.get('feature', '?')}\")
print(f\"Variations: {state.get('count', '?')}\")
print()
port = state.get('port_start', 3001)
for key, val in sorted(state.get('variations', {}).items()):
    status = val.get('status', 'unknown')
    constraint = val.get('constraint', '?')
    print(f'  {key}: port {port} | {status} | {constraint} | {val.get(\"worktree\", \"?\")}')
    port += 1
" "$STATE_FILE" 2>/dev/null
        ;;

    *)
        echo "Usage: port-manager.sh {find|kill-all|status} [args]"
        exit 1
        ;;
esac

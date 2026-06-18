#!/usr/bin/env bash
# wake-signals.sh - log-only observation of kind=question wake-signals.
#
# Lifted from hooks/target-stop-hook.sh (Phase 1 of stop-hook refactor).
# Reads kind=question wake-signals from .fno/wake-signals/ and
# appends one wake_signal_observed JSON line per signal to
# hook-events.jsonl. LOG-ONLY: does NOT delete signals (SessionStart /
# UserPromptSubmit own that). Does NOT refuse exit or trip any detector.
#
# Graceful-degrade: a missing fno.wake.signal module is silently
# skipped so sessions without the wake substrate are unaffected.
#
# Requires (set by caller):
#   SCRIPT_DIR - path to the abilities plugin root (parent of scripts/)
#   REPO_ROOT  - project repo root (current working tree)
#   STATE_DIR  - .fno/ directory for the project

_observe_wake_signals() {
    local cli_dir="$SCRIPT_DIR/cli"
    local sigs
    # REPO_ROOT is passed via env var (not shell-interpolated into the Python string
    # literal) so paths containing single-quotes do not cause a SyntaxError.
    sigs=$(REPO_ROOT="$REPO_ROOT" uv run --project "$cli_dir" python3 -c "
import json, os
from pathlib import Path
from fno.wake.signal import read_signals
print(json.dumps(read_signals(Path(os.environ['REPO_ROOT']), kind='question')))
" 2>/dev/null) || return 0

    [[ "$sigs" == "[]" ]] && return 0

    echo "$sigs" | uv run --project "$cli_dir" python3 -c "
import json, sys, time
for sig in json.load(sys.stdin):
    print(json.dumps({
        'event': 'wake_signal_observed',
        'signal_id': sig.get('signal_id', ''),
        'kind': sig.get('kind', ''),
        'from': sig.get('from_project', ''),
        'msg_id': sig.get('msg_id', ''),
        'ts': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
    }))
" >> "$STATE_DIR/hook-events.jsonl" 2>/dev/null || return 0
}

#!/usr/bin/env bash
# Stop hook: capture skill-worthy signals from each session transcript.
#
# Cross-platform: Claude Code (Stop), Codex CLI (Stop), Gemini CLI (AfterAgent).
# Runs on every session stop. Lightweight - just extracts metadata from the
# JSONL transcript and appends to a signals file. No LLM calls.
#
# Signals captured:
# - Repeated tool patterns (same tool sequence used 3+ times)
# - Multi-step workflows (5+ tool calls in a coherent sequence)
# - User corrections ("no", "don't", "instead", "actually")
# - Explicit skill requests ("make this a skill", "remember this workflow")
#
# Output: appends to .fno/convo-signals.jsonl (project) and
#         ~/.fno/convo-signals.jsonl (global)

set -uo pipefail

# ── Platform detection ───────────────────────────────────────────────────
detect_platform() {
    if [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
        echo "gemini"
    elif [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
        echo "codex"
    elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
        echo "claude"
    else
        echo "claude"
    fi
}
PLATFORM=$(detect_platform)

# Read hook input from stdin
INPUT=$(cat)

# Extract transcript path from hook input (all platforms provide this)
TRANSCRIPT_PATH=$(echo "$INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data.get('transcript_path', ''))
except:
    pass
" 2>/dev/null)

# Fallback: find most recent transcript (Claude Code only)
if [[ -z "$TRANSCRIPT_PATH" ]] || [[ ! -f "$TRANSCRIPT_PATH" ]]; then
    if [[ "$PLATFORM" == "claude" ]]; then
        TRANSCRIPT_PATH=$(find "$HOME/.claude/projects" -name '*.jsonl' -type f -exec ls -t {} + 2>/dev/null | head -1)
    fi
fi

[[ -z "$TRANSCRIPT_PATH" ]] && exit 0
[[ ! -f "$TRANSCRIPT_PATH" ]] && exit 0

# Determine output location. Source paths.sh for STATE_DIR if available.
if command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
PROJECT_SIGNALS=".fno/convo-signals.jsonl"
GLOBAL_SIGNALS="${STATE_DIR:-$HOME/.fno}/convo-signals.jsonl"
mkdir -p "$(dirname "$PROJECT_SIGNALS")" 2>/dev/null || true
mkdir -p "$(dirname "$GLOBAL_SIGNALS")" 2>/dev/null || true

# Extract signals using jq (fast, no LLM needed)
python3 -c "
import json, sys, os
from datetime import datetime
from collections import Counter

transcript_path = sys.argv[1]
project_signals = sys.argv[2]
global_signals = sys.argv[3]
platform = sys.argv[4] if len(sys.argv) > 4 else 'claude'

lines = []
try:
    with open(transcript_path) as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    lines.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
except Exception:
    sys.exit(0)

if len(lines) < 4:
    sys.exit(0)

signals = []
session_id = os.path.basename(transcript_path).replace('.jsonl', '')
ts = datetime.utcnow().isoformat() + 'Z'

# 1. Extract tool usage patterns
tool_sequences = []
current_seq = []
for entry in lines:
    if entry.get('type') == 'assistant':
        content = entry.get('message', {}).get('content', [])
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get('type') == 'tool_use':
                    tool_name = block.get('name', '')
                    current_seq.append(tool_name)
        elif current_seq:
            tool_sequences.append(tuple(current_seq))
            current_seq = []

if current_seq:
    tool_sequences.append(tuple(current_seq))

# Find repeated tool patterns (3+ occurrences of same tool combo)
tool_combos = Counter()
for seq in tool_sequences:
    # Look at sliding windows of 2-4 tools
    for window_size in range(2, min(5, len(seq) + 1)):
        for i in range(len(seq) - window_size + 1):
            combo = seq[i:i + window_size]
            tool_combos[combo] += 1

for combo, count in tool_combos.items():
    if count >= 3:
        signals.append({
            'ts': ts,
            'session_id': session_id,
            'type': 'repeated_tool_pattern',
            'pattern': list(combo),
            'count': count,
            'source': 'auto'
        })

# 2. Extract user text for correction patterns and explicit requests
user_texts = []
for entry in lines:
    if entry.get('type') == 'user':
        content = entry.get('message', {}).get('content', '')
        if isinstance(content, str):
            user_texts.append(content)
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get('type') == 'text':
                    user_texts.append(block.get('text', ''))

# Check for correction patterns
correction_keywords = ['no not that', 'don\\'t', 'stop doing', 'instead', 'actually,', 'wrong', 'not what i']
corrections = []
for text in user_texts:
    lower = text.lower()
    for kw in correction_keywords:
        if kw in lower:
            corrections.append(text[:200])
            break

if len(corrections) >= 2:
    signals.append({
        'ts': ts,
        'session_id': session_id,
        'type': 'repeated_corrections',
        'count': len(corrections),
        'samples': corrections[:3],
        'source': 'auto'
    })

# 3. Check for explicit skill requests
skill_keywords = ['make this a skill', 'remember this', 'save this workflow', 'create a skill', 'automate this']
for text in user_texts:
    lower = text.lower()
    for kw in skill_keywords:
        if kw in lower:
            signals.append({
                'ts': ts,
                'session_id': session_id,
                'type': 'explicit_skill_request',
                'text': text[:300],
                'source': 'user'
            })
            break

# 4. Session summary signal (always capture for pattern analysis)
assistant_texts = []
for entry in lines:
    if entry.get('type') == 'assistant':
        content = entry.get('message', {}).get('content', [])
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get('type') == 'text':
                    assistant_texts.append(block.get('text', ''))

# Count unique tools used
all_tools = []
for entry in lines:
    if entry.get('type') == 'assistant':
        content = entry.get('message', {}).get('content', [])
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get('type') == 'tool_use':
                    all_tools.append(block.get('name', ''))

tool_counts = Counter(all_tools)

# First user message as topic hint
topic = user_texts[0][:150] if user_texts else ''

signals.append({
    'ts': ts,
    'session_id': session_id,
    'type': 'session_summary',
    'platform': platform,
    'topic': topic,
    'turns': len([e for e in lines if e.get('type') in ('user', 'assistant')]),
    'tool_usage': dict(tool_counts.most_common(10)),
    'user_messages': len(user_texts),
    'source': 'auto'
})

# Write signals
if signals:
    # Write to project signals (if in a project dir)
    try:
        with open(project_signals, 'a') as f:
            for sig in signals:
                f.write(json.dumps(sig) + '\\n')
    except Exception:
        pass

    # Also write to global signals
    try:
        with open(global_signals, 'a') as f:
            for sig in signals:
                f.write(json.dumps(sig) + '\\n')
    except Exception:
        pass
" "$TRANSCRIPT_PATH" "$PROJECT_SIGNALS" "$GLOBAL_SIGNALS" "$PLATFORM"

# ── Bounded rotation ─────────────────────────────────────────────────────
# convo-signals.jsonl is the largest unbounded append log in .fno/. Cap it
# from config.logs.convo_signals_max_mb (default 5 MB); a missing/garbage
# value degrades to the default and never raises.
_HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_ROTATE="$_HOOK_DIR/../scripts/lib/rotate-append-log.sh"
if [[ -f "$_ROTATE" ]]; then
    _cap_mb=""
    for _sf in ".fno/settings.yaml" "${STATE_DIR:-$HOME/.fno}/settings.yaml"; do
        [[ -f "$_sf" ]] || continue
        _cap_mb="$(grep -E '^[[:space:]]*convo_signals_max_mb:' "$_sf" 2>/dev/null | head -1 | sed -E 's/.*:[[:space:]]*//; s/["'\'']//g; s/[[:space:]]*#.*//; s/[[:space:]]*$//')"
        [[ -n "$_cap_mb" ]] && break
    done
    [[ "$_cap_mb" =~ ^[0-9]+$ ]] || _cap_mb=5
    # Force base-10: a leading-zero value (e.g. 08/09) is invalid octal and
    # would make this arithmetic fail, silently disabling rotation.
    _cap_bytes=$(( 10#$_cap_mb * 1024 * 1024 ))
    bash "$_ROTATE" "$PROJECT_SIGNALS" "$_cap_bytes" 2>/dev/null || true
    bash "$_ROTATE" "$GLOBAL_SIGNALS" "$_cap_bytes" 2>/dev/null || true
fi

exit 0

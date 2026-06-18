---
name: cache-keepalive
description: "Keep prompt cache alive during idle. Prevents 10x cost spike when cache expires. Use when: 'keep cache warm', 'cache keepalive', or auto-activated at session start when project opts in."
---

# Cache Keepalive

Schedules 4 pings via ScheduleWakeup to keep prompt cache warm during idle periods. Self-terminates after ~18 minutes. Max cost: ~$0.52.

ScheduleWakeup resumes the same conversation, which keeps cached tokens alive by definition. Each ping is a lightweight wake that re-reads the cached context and reschedules.

## Configuration

Project opt-in via `.claude/settings.local.json`:

```json
{
  "cacheKeepalive": true
}
```

When enabled, the SessionStart hook auto-activates keepalive silently.
When absent or false, keepalive is manual-only (`/cache-keepalive`).

### Quick Setup

`/cache-keepalive config claude` - enables auto-activation for the current project:

```bash
mkdir -p .claude
SETTINGS=".claude/settings.local.json"
if [[ -f "$SETTINGS" ]]; then
  jq '.cacheKeepalive = true' "$SETTINGS" > "${SETTINGS}.tmp" && mv "${SETTINGS}.tmp" "$SETTINGS"
else
  echo '{"cacheKeepalive": true}' | jq . > "$SETTINGS"
fi
```

Confirm with: `Cache keepalive auto-activation enabled for this project. Takes effect next session.`

To disable: `/cache-keepalive config claude off`

```bash
SETTINGS=".claude/settings.local.json"
if [[ -f "$SETTINGS" ]]; then
  jq '.cacheKeepalive = false' "$SETTINGS" > "${SETTINGS}.tmp" && mv "${SETTINGS}.tmp" "$SETTINGS"
fi
```

## Process

### 1. Get Cache Stats

```bash
SESSION_ID=$(cat ~/.claude/.session-context.json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('session_id',''))" 2>/dev/null)
if [ -n "$SESSION_ID" ]; then
  find ~/.claude/projects/ -name "${SESSION_ID}.jsonl" 2>/dev/null | head -1 | xargs python3 -c "
import json, sys
last_usage = None
model = 'unknown'
for line in open(sys.argv[1]):
    try:
        obj = json.loads(line)
        if obj.get('type') == 'assistant':
            u = obj.get('message', {}).get('usage', {})
            m = obj.get('message', {}).get('model', '')
            if u.get('cache_read_input_tokens', 0) > 0:
                last_usage = u
                model = m
    except: pass
if last_usage:
    cr = last_usage.get('cache_read_input_tokens', 0)
    inp = last_usage.get('input_tokens', 0)
    cc = last_usage.get('cache_creation_input_tokens', 0)
    total_ctx = cr + inp + cc
    print(f'CACHE_READ={cr}')
    print(f'INPUT={inp}')
    print(f'TOTAL_CTX={total_ctx}')
    print(f'MODEL={model}')
else:
    print('NO_USAGE_DATA')
" 2>/dev/null
fi
```

Pricing per million tokens (use ONLY these values):
- Opus 4.5/4.6: input=$5.00, cache_read=$0.50
- Opus 4.0/4.1: input=$15.00, cache_read=$1.50
- Sonnet (all): input=$3.00, cache_read=$0.30
- Haiku 4.5: input=$1.00, cache_read=$0.10

Calculate: uncached = TOTAL_CTX / 1M * input_price. cached = TOTAL_CTX / 1M * cache_read_price.

### 2. Report and Schedule First Ping

Report cache stats to user:

```
Cache keepalive active.
  Context: ~[X]K tokens ([MODEL])
  Cache miss would cost: ~$[uncached] | Cached: ~$[cached] | Savings: ~$[diff]

  Schedule: 4 pings at ~270s intervals (~18 min total)
  Total keepalive cost: ~$0.52

  Cancel: just type anything (user input naturally cancels the loop)
  Only fires when idle. Normal work refreshes cache automatically.
```

Then schedule the first ping:

```
ScheduleWakeup({
  delaySeconds: 270,
  reason: "cache keepalive ping 1/4 - keeping prompt cache warm",
  prompt: "/cache-keepalive"
})
```

### 3. On Each Wake (pings 1-4)

Determine current ping number by checking conversation for the most recent `[cache-keepalive] Ping N/4` message. If none found, this is ping 1.

Output ONLY:

```
[cache-keepalive] Ping N/4 | cache warm
```

Then:

- **Pings 1-2:** Schedule next ping with ScheduleWakeup at 270s
- **Ping 3:** Send OS notification warning, then schedule final ping:
  ```bash
  if [[ "$(uname)" == "Darwin" ]]; then
    osascript -e 'display notification "Return to your session or the cache will expire on next ping" with title "Cache Keepalive"' 2>/dev/null
  elif command -v notify-send &>/dev/null; then
    notify-send "Cache Keepalive" "Return to your session or the cache will expire on next ping" 2>/dev/null
  fi
  ```
- **Ping 4:** Final warning, stop scheduling (let cache expire gracefully):
  ```
  [cache-keepalive] Ping 4/4 | final ping, cache protection ending. Type anything to continue working.
  ```
  Do NOT call ScheduleWakeup after ping 4.

### 4. Cancellation

User input at any point naturally cancels the ScheduleWakeup loop. No explicit cancel handler needed.

## NEVER

- NEVER use CronCreate (fires new sessions, doesn't keep current cache warm)
- NEVER more than 4 pings per activation
- NEVER read files or run other tools (except ScheduleWakeup) during pings
- NEVER block user input (ScheduleWakeup yields to user naturally)
- NEVER activate without project opt-in (when auto-activated via hook)

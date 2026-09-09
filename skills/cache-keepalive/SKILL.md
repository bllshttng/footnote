---
name: cache-keepalive
description: "Keep prompt cache alive during idle. Prevents 10x cost spike when cache expires. Use when: 'keep cache warm', 'cache keepalive', or auto-activated at session start when project opts in."
metadata:
  requires:
    harness:
      - claude
---

# Cache Keepalive

Schedules 4 pings via ScheduleWakeup to keep the prompt cache warm during idle periods. Self-terminates after ~18 minutes.

**Capability gate.** The scheduler is Claude's ScheduleWakeup. When it is unavailable, report `schedule unavailable` and stop - never claim warmth and never substitute another timer (CronCreate fires new sessions and does not keep this cache warm).

**Warmth is measured, never assumed.** A scheduled ping re-reads cached context, which makes the context cache-ELIGIBLE; it does not prove a cache HIT. Warmth is proven only by usage data (`cache_read_input_tokens > 0` in the transcript's last assistant usage). Report `warmth unverified` unless that number proves a cache read, and report the measured numbers rather than a defined-true claim.

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

Pricing: resolve input and cache-read prices for the reported model from the provider's current pricing page at report time, and say the numbers were looked up then. Prices drift and were not independently measured here; when you cannot look them up, report `pricing unverified` and skip the cost comparison instead of quoting a stored literal.

Calculate: uncached = TOTAL_CTX / 1M * input_price. cached = TOTAL_CTX / 1M * cache_read_price.

### 2. Report and Schedule First Ping

Report cache stats to user:

```
Cache keepalive active.
  Context: ~[X]K tokens ([MODEL])
  Cache miss would cost: ~$[uncached] | Cached: ~$[cached] | Savings: ~$[diff]

  Schedule: 4 pings at ~270s intervals (~18 min total), armed via ScheduleWakeup
  Keepalive cost: negligible (each ping is a cached-context re-read)

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
[cache-keepalive] Ping N/4 | cache eligible (warmth unverified unless usage proves a cache read)
```

Then:

- **Pings 1-2:** Schedule next ping with ScheduleWakeup at 270s
- **Ping 3:** Send the OS notification warning - the ONE sanctioned shell action during a ping - then schedule the final ping:
  ```bash
  if [[ "$(uname)" == "Darwin" ]]; then
    osascript -e 'display notification "Return to your session or the cache will expire on next ping" with title "Cache Keepalive"' 2>/dev/null
  elif command -v notify-send &>/dev/null; then
    notify-send "Cache Keepalive" "Return to your session or the cache will expire on next ping" 2>/dev/null
  fi
  ```
  If neither notifier exists, skip it silently; a missing notifier is not a keepalive failure.
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
- NEVER read files or run other tools during pings - the two exceptions are ScheduleWakeup itself and the ping-3 OS notification
- NEVER block user input (ScheduleWakeup yields to user naturally)
- NEVER activate without project opt-in (when auto-activated via hook)
- NEVER report "cache warm" without usage data proving a cache read

## Known Limitations and Deferred Work

- Keepalive cannot restore an expired cache or session. See [LIMITATIONS.md](LIMITATIONS.md).

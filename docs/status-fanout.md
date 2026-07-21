# Status fanout

Fan out task and run events to an external channel (Discord, Slack, n8n, Notion, ...) so you get pinged when a run finishes or wedges without watching a terminal.

## The model

Workers append protocol-family events (`task_started`, `task_done`, `blocked`, `run_summary`) to `.fno/events.jsonl` and never know sinks exist.
A tick sweeps that log and POSTs each matching event to every configured sink.
Sinks are pure config: add one to `config.status_sinks`, add none and nothing is sent.

Two ways the tick runs:

- **By hand:** `fno status-fanout tick` runs one pass for the current project.
- **By the daemon:** the `fno-agents` daemon discovers every project with an enabled sink and ticks each on its own `status_fanout.interval_secs` (default 5s).

## Configuring a sink

```toml
[[status_sinks]]
name = "discord"                       # keys .fno/status-sinks/<name>.cursor; must be filesystem-safe
type = "text-webhook"                  # json-webhook | text-webhook | backlog-progress
url_env = "FNO_STATUS_DISCORD"         # the webhook secret (see below); or inline url = "..."
field = "content"                      # text-webhook: the message field (Discord=content, Slack=text)
events = ["run_summary", "blocked"]    # route only these types; empty = every event (a firehose)
template = "fno [{project}] {type} {outcome} - {data.reason}"
enabled = true

[status_fanout]
interval_secs = 5
http_timeout_secs = 5
retries = 2
```

### Sink types

- **`text-webhook`** renders `template` per event and POSTs `{field: rendered}`.
  One adapter serves Discord (`field = "content"`), Slack-incoming (`field = "text"`), and ntfy.
  `field = "content"` also sends `allowed_mentions: {"parse": []}`, so a worker-controlled reason containing `@everyone` cannot ping the channel; other fields defang Slack broadcast tokens.
- **`json-webhook`** POSTs the raw event JSON (optionally CloudEvents-wrapped via `cloudevents = true`).
  The escape hatch for n8n / Zapier / a custom receiver.
- **`backlog-progress`** appends a progress note to the event's backlog node and its plan doc.

### The secret: `url_env` vs `url`

Prefer `url_env` over an inline `url`.
An inline `url` lands the webhook in `config.toml`, which typically syncs; `url_env` keeps the secret out of it.

`url_env` resolves from the process environment first, then from `~/.fno/.env`.
This ordering matters for the daemon: it ticks with its own environment and never saw your `export FNO_STATUS_DISCORD=...`, so an exported-only secret is invisible to it and every daemon tick short-circuits on `url_env ... unset`.
Put the value in `~/.fno/.env` (`FNO_STATUS_DISCORD=https://...`) for unattended delivery; an exported process-env value still wins when present.

## Delivery semantics

- **Fresh sink starts at EOF.** A newly configured sink only ever sees events appended after its first tick, so it never replays history. To smoke-test a sink, tick once to prime the cursor, then emit a test event (`fno event emit --type blocked --data '{"reason":"test"}'`), then tick again to deliver.
- **At-least-once with retry.** A connect-class failure or a 5xx/429/401/403/408 holds the cursor and retries next tick (bounded by `retries`); a permanent 4xx drops the event and advances.
  Drops and short-circuits are logged to `.fno/status-sinks/<name>.errors.jsonl` (the var name, never its value).
- **Discord User-Agent.** Every webhook POST sends an explicit `User-Agent`; Discord 403s the stdlib default `Python-urllib`, so without it a Discord sink would never deliver.

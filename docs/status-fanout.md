# Status fanout

Fan out task and run events to an external channel (Discord, Slack, n8n, Notion, ...) so you get pinged when a run finishes or wedges without watching a terminal.

## The model

Workers append protocol-family events (`task_started`, `task_done`, `blocked`, `run_summary`) to `.fno/events.jsonl` and never know sinks exist.
A tick sweeps that log and POSTs each matching event to every configured sink.
Sinks are pure config: add one to `config.status_sinks`, add none and nothing is sent.

Two ways the tick runs:

- **By hand:** `fno doctor event fanout tick` runs one pass for the current project.
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

- **`text-webhook`** renders `template` per event and POSTs `{field: rendered}`. One adapter serves Discord (`field = "content"`) and Slack-incoming (`field = "text"`). ntfy instead sets `raw_body = true`. With a topic in the URL, the body IS the raw message. A JSON envelope shows as literal JSON. `field = "content"` also sends `allowed_mentions: {"parse": []}`, so a worker-controlled reason containing `@everyone` cannot ping the channel. Other fields defang Slack broadcast tokens.
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

- **Fresh sink starts at EOF.** A new sink sees only events appended after its first tick. It never replays history. To test a sink, tick once to prime the cursor. Then emit an event (`fno doctor event emit --type blocked --data '{"reason":"test"}'`) and tick again.
- **At-least-once with retry.** A connect-class failure or a 5xx/429/401/403/408 holds the cursor and retries next tick (bounded by `retries`); a permanent 4xx drops the event and advances.
  Drops and short-circuits are logged to `.fno/status-sinks/<name>.errors.jsonl` (the var name, never its value).
- **Discord User-Agent.** Every webhook POST sends an explicit `User-Agent`; Discord 403s the stdlib default `Python-urllib`, so without it a Discord sink would never deliver.

## Reaching a remote operator (`operator_notice`)

`fno inbox notify TITLE BODY` (and every automatic caller behind `send_notification`) also appends an `operator_notice` event to the project journal. A sink that routes that type carries the notice off the host, so an operator who is not at the machine still sees it:

```toml
[[status_sinks]]
name = "phone"
type = "text-webhook"
url_env = "FNO_STATUS_PHONE"          # e.g. an ntfy topic URL or a Discord webhook
raw_body = true                        # ntfy needs this; a Discord webhook does not
field = "content"
events = ["operator_notice"]
template = "fno [{project}] {data.title} - {data.body} ({data.pointer})"
enabled = true
```

The arm notices ride this sink too. `arm_watch`, the `notify_watch` board and main-CI lanes, and the `provider_cap` notices all land as `operator_notice` rows. `arm_watch` names arms broken past `[notify] arm_failing_after_s`, plus hung verbs and dead flight holders.

In the Rust callers `--pointer` leads the argv: `fno inbox notify --pointer P TITLE BODY`. The Python group callback refuses an option after its positionals, so the trailing form exits 2 and writes no row. `arm_watch` counts a notice sent only on exit 0. A notice that died at the gate leaves the dedupe token unwritten, so the next tick retries.

### The ntfy recipe (self-hosted over Tailscale)

The sink URL is `https://<host>:8443/<topic>` and the rendered template is the whole POST body. Keep `raw_body = true`. Measured on ntfy 2.28.0: any JSON envelope posted to a topic URL reaches the phone as literal JSON.

One macOS recipe that works end to end:

- Build the server binary yourself: the Homebrew formula and the darwin release image are client-only. The goreleaser config builds darwin with the `noserver` tag. Clone the repo at your version tag and run `go build -tags sqlite_omit_load_extension`. cgo is required, the sqlite cache dies under `CGO_ENABLED=0`.
- Config at `/opt/homebrew/etc/ntfy/server.yml` with `listen-http 127.0.0.1:2586`, `behind-proxy true`, and a `cache-file` under the fno state dir. Set `base-url` to the public HTTPS URL. Set `upstream-base-url` to `https://ntfy.sh` so iOS pushes instantly. ntfy.sh receives only a topic-hash poll request. The phone fetches the content from your server.
- Run it from a user LaunchAgent with `KeepAlive` and `RunAtLoad` so it survives reboots. `ProgramArguments` points at the built binary with `serve --config <path>`.
- Expose it tailnet-only with `tailscale serve --bg --https=8443 http://127.0.0.1:2586`. Never `tailscale funnel`: the server carries no auth of its own. The tailnet boundary is the access control, and no token enters the repo or the config files.
- Set `FNO_STATUS_PHONE=https://<host>:8443/<topic>` in `~/.fno/.env` so the daemon resolves it. Subscribe the phone app to that exact URL, and keep `base-url` matching it. Verify with one `fno doctor event emit` plus a fanout tick.

A notice is a pointer, never a second inbox. `data.pointer` names the verb that shows the durable state (`fno inbox outstanding`, `fno inbox board`). `data.body` carries counts, never queue rows. With no sink configured, nothing leaves the host.

Badge notices. A blocked badge (a permission prompt or an idle wait) rides this lane, so it reaches the phone. A done badge is a turn end. With `mux.notify_on_done` on, it fires a local toast only and writes no `operator_notice` row. A crowned king's done sends nothing. `scripts/probes/phone-notice-noise-probe.sh` counts the notices in a window.

Automatic sampling of the king board, the court and main CI is a planned follow-up; today the notice fires from the existing `notify` callers.

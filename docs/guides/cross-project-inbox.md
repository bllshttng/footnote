# Cross-project inbox guide

Send messages between agents in different projects without losing context across iterations.

## Onboarding a project to the inbox fleet

Run this 4-step checklist once per project to opt that project into the fleet so peer projects can `fno mail send --to-project <you>` and your daemon will react automatically. Steps must run in order; step 3 errors loudly if step 2 was skipped.

**1. Set the project name.** Edit `<repo>/.fno/settings.yaml` and add (or confirm) `project: <name>` at the top. The name MUST match what peers will use in `fno mail send --to-project <name>` (example known names: `footnote`, `acme-backend`, `acme-web`, `acme-docs`, `acme-blog`, `marketing`). If the field is missing, every `fno mail` verb errors with `set 'project:' in .fno/settings.yaml or pass --from` (recipient verbs keep `--from`; the send verb is now `fno mail send --from-name`).

**2. Enable the watch daemon.** In the same `.fno/settings.yaml`, set `config.inbox.watch.enabled: true`. The flag is opt-in by design: a project that does not flip it stays in megawalk-Step-0-only mode and never spawns a launchd agent.

**3. Install the launchd entry.** Run `fno watch install` from the project directory. This creates `~/Library/LaunchAgents/com.fno.watch.<project>.plist`, loads it via `launchctl`, and starts the daemon. If step 2 was skipped, `fno watch install` refuses with a clear error pointing back to step 2.

**4. Confirm the daemon is healthy.** Run `fno watch status`. Expect `loaded: yes` plus the most recent line from `<repo>/.fno/abi-watch.log`. If `loaded: no`, run `fno watch install` again or check stderr for the launchctl error. For a richer health snapshot, run `fno mail status` (described in the [Status](#status) section below).

### Onboarding troubleshooting

Three failure modes cover most onboarding blockers:

- **`fswatch` not on PATH.** The daemon shells out to `fswatch` and exits silently if it is missing. Fix: `brew install fswatch`. macOS only - other platforms cannot run the daemon at all.
- **`claude` not authenticated for headless use.** The daemon spawns `claude -p --bare` to drain heads-ups; if `claude` is not logged in, the spawn errors and writes to `<repo>/.fno/inbox-errors.jsonl`. Fix: run `claude /login` once, then smoke-test with `claude -p --bare 'echo ok'`.
- **`~/.fno/inbox-drain-prompt.md` missing.** The drain reads the system prompt from this file. Fix: `bash scripts/install-drain-prompt.sh` (idempotent; never overwrites an existing customized prompt).

## Setup (one-time per project)

Each project must declare its identity in `.fno/settings.yaml`:

```yaml
project: acme-web
```

Without this field, `fno mail` errors loudly. There is no fallback to cwd basename; the project name must be explicit.

Optional rotation settings:

```yaml
config:
  inbox:
    auto_rotate: true
    max_size_bytes: 1048576      # 1 MB
    max_read_messages: 200
    keep_recent_read: 50
    triage:
      timeout_sec: 60
      log_decisions: true
```

## Send a message

```bash
fno mail send --to-project acme-web --kind heads-up \
    --body "New region data source live in PR 112"
```

Five message kinds are available:

- `question` - Ask something. Interrupts the recipient's mid-feature work.
- `answer` - Reply to a question. Threads via `reply_to`.
- `heads-up` - "This might affect you, you decide what to do." Triggers LLM triage on the recipient side.
- `notification` - Pure FYI, no action expected.
- `lesson` - Cross-project memory write (typically supervisor -> worker).

Optional flags: `--reply-to <msg-id>`, `--ref-pr <N>`, `--ref-node ab-...`, `--ref-gate <name>`.

## Read your inbox

```bash
fno mail unread             # list unread messages (table format)
fno mail unread --json       # machine-readable
fno mail list --all          # full history including read messages
```

## Acknowledge a message

```bash
fno mail ack msg-a4f1b2 [--triaged-into ab-c93b1234]
```

The `--triaged-into` flag links the inbox message to the graph node it produced (only relevant for triaged heads-ups).

## Reply to a message

```bash
fno mail reply --to msg-a4f1b2 --kind answer \
    --body "silent-failure-hunter HIGH on swallow_errors_in_dispatch.py"
```

The reply lands in the original sender's inbox with `reply_to` set. If the msg-id is unknown, the reply lands in your own inbox as a self-note (you'll see `wrote orphan reply ... to own inbox` instead of `sent reply ...`).

## LLM triage (heads-ups only)

```bash
fno mail triage msg-a4f1b2 --json
```

Returns a structured plan:

```json
{
  "action": "create_node",
  "title": "Add region filter",
  "priority": "p2",
  "body": "...",
  "follow_up_question": null
}
```

Three actions: `create_node` (becomes graph entry via `fno new --source-*`), `ignore` (just ack), `request_clarification` (send back as `kind: question`).

## Where messages live (post-2026-05 thread-per-file layout)

- Active threads: `~/your-vault/internal/agents/{project}/inbox/{YYYY-MM-DD}-{slug}.md`
  (one file per thread; replies append to the same file)
- Archive (after rotation): `~/your-vault/internal/agents/{project}/inbox/archive/{YYYY-MM}/`
- Pre-2026-05 safety net (post-migration): `~/your-vault/internal/agents/{project}/inbox-pre-migration.md`
- Errors (malformed threads, dispatch failures): `.fno/inbox-errors.jsonl`
- Triage decisions log: `.fno/triage-log.jsonl`

A thread is "unread" when its frontmatter has no `read_at:` field.
Drain sets `read_at:` after dispatch (except for `kind: question`,
which intentionally stays unread until a human handles it).

## Migrating from the pre-2026-05 flat layout

If you have existing flat-file inboxes at
`~/your-vault/internal/agents/*/inbox.md`, run the one-shot migration:

```bash
python3 scripts/migrate-inbox-flat-to-threads.py --dry-run   # preview
python3 scripts/migrate-inbox-flat-to-threads.py             # apply
```

The script splits each `inbox.md` into one thread file per
conversation, collapses `reply_to:` chains into a single thread, maps
removed kinds to their replacements (notification -> fyi, lesson -> fyi
with persist_to_memory, answer/complete -> fyi with replies_to), and
renames the original file to `inbox-pre-migration.md` as a safety net.
Idempotent: re-running on already-migrated projects is a no-op.

The `com.fno.backlog-sync` launchd job mirrors `~/.fno/graph.json` to obsidian; the inbox files already live in the obsidian vault, so no separate sync is needed.

## Megawalk integration

Megawalk drains the inbox automatically at the top of every iteration. Only `kind: question` interrupts mid-feature work; all other kinds wait until between features so cross-project chatter cannot derail focused work.

See `skills/megawalk/references/inbox-handlers.md` for the per-kind dispatch table.

## Linting and recovering from corruption

```bash
fno mail lint acme-web
```

Reports any malformed message blocks, with line numbers, and exits non-zero if any errors are found. The malformed blocks themselves are skipped during normal `unread`/`list` calls (they appear in `inbox-errors.jsonl` but not in the user-facing output).

## Enable headless drain

The headless drain daemon watches your inbox file with `fswatch` and invokes `claude -p` to triage new messages automatically - no open Claude Code session required.

### Prerequisites

- **macOS only.** The daemon uses `launchd`, which is Apple-specific. On Linux or Windows you can still use megawalk's Step 0 inbox drain and manual `fno mail drain` calls; you just lose the autonomous-on-fswatch trigger.
- `fswatch` on PATH: `brew install fswatch`
- `claude` (Claude Code CLI) on PATH and authenticated. Run `claude /login` if you have not authenticated yet.

### Install steps

**1. Enable the watch flag in your project settings.**

Add (or set) in `<repo>/.fno/settings.yaml`:

```yaml
config:
  inbox:
    watch:
      enabled: true
```

You can also run `/setup` in Claude Code for an interactive wizard that writes this for you.

**2. Install the system prompt.**

```bash
bash scripts/install-drain-prompt.sh
```

This writes `~/.fno/inbox-drain-prompt.md` if it does not already exist. The script is idempotent - if the file is already there, it is not overwritten, so any customizations you have made are preserved.

**3. Register the launchd agent.**

```bash
fno watch install
```

This creates and loads `~/Library/LaunchAgents/com.fno.watch.{project}.plist`, where `{project}` is the value of the `project:` field in your project's `settings.yaml`. The agent starts immediately and survives reboots.

**4. Verify the agent is running.**

```bash
fno watch status
```

Expect output like:

```
loaded: yes
last_event: 2026-05-05T18:42:01Z  inbox.md modified
```

If `loaded: no`, check that `fswatch` is on PATH and that `config.inbox.watch.enabled` is set.

### Disabling

Two paths depending on how permanent you want the removal:

**Full removal** - removes the plist and unloads the launchd entry:

```bash
fno watch uninstall
```

The daemon stops and will not restart on reboot. To re-enable, run `fno watch install` again.

**One-off pause** - unloads without deleting the plist:

```bash
launchctl unload ~/Library/LaunchAgents/com.fno.watch.{project}.plist
```

The plist file remains. Reload later with:

```bash
launchctl load ~/Library/LaunchAgents/com.fno.watch.{project}.plist
```

### Notification policy

When the daemon handles a message, it can fire a macOS notification on the sender side so you know the message went out. The behavior is controlled by `config.inbox.watch.notify_on_send` in the sending project's `settings.yaml`:

| Value | Behavior |
|-------|----------|
| `"question_only"` (default) | notification fires only when the sender uses `--kind question` |
| `"all"` | every send fires a notification |
| `"off"` | no notifications |

The notification fires on the **sender** side - the project that ran `fno mail send`. macOS only; non-darwin platforms skip it silently.

To suppress all notifications:

```yaml
config:
  inbox:
    watch:
      notify_on_send: "off"
```

### Active-session bypass

The daemon does not spawn a competing `claude -p` process if the target project already has an active session:

- An active target session: `target-state.md` mtime within 5 minutes AND `status: IN_PROGRESS`
- An active interactive Claude Code session: the project's transcript jsonl mtime within 5 minutes

In either case the daemon drops a wake signal instead. The active session's hooks consume it on the next user-prompt turn or on the next session boot. This means "I sent a message but nothing happened immediately" is expected behavior when you or target are actively working in the target project - the message is not lost, it will be picked up at the next natural pause.

## Status

`fno mail status` prints a one-screen health snapshot for the current project. Useful when you want to know "is my daemon doing work" without grepping three log files.

```bash
fno mail status
```

```
project: footnote
daemon: loaded
inbox path: /Users/me/your-vault/internal/agents/footnote/inbox
unread: 2
acked_24h: 7
last drain: 4m ago
active session: idle
wake signals: 0
errors_24h: 0
```

Add `--json` for machine-readable output. The same eight fields appear as top-level keys in the JSON object.

| Field | Meaning |
|-------|---------|
| `daemon` | `loaded` if `launchctl` knows the per-project agent, else `not_installed` |
| `inbox_path` | Absolute path to the project's `inbox/` directory |
| `unread` | Count of unread messages |
| `acked_24h` | Count of `read` messages with timestamp within the last 24 hours |
| `last_drain` | Relative time of the most recent `drain complete` line in `abi-watch.log` (`4m ago`, `2h ago`, `never`) |
| `active_session` | `idle`, `target_active`, or `interactive_active` (matches the daemon's bypass logic) |
| `wake_signals` | Count of pending files in `<repo>/.fno/wake-signals/` |
| `errors_24h` | Count of `inbox-errors.jsonl` entries with a parseable `ts` within the last 24 hours |

`fno mail status` is informational and does not require the daemon to be loaded - it is safe to run even before step 3 of the [onboarding checklist](#onboarding-a-project-to-the-inbox-fleet).

## Architecture reference

Full architecture, rotation algorithm, and design rationale: see [docs/architecture/cross-project-inbox.md](../architecture/cross-project-inbox.md).

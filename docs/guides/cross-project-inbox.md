# Cross-project inbox guide

Send messages between agents in different projects without losing context across iterations.

> **Note (2026-06):** the headless `fno watch` launchd drain daemon, its `fswatch` loop, and archive rotation were removed. The cross-session relay supersedes the autonomous-push use case; recipients now drain with `fno mail drain` (run manually or by an autonomous worker). Sections describing `fno watch install/status/uninstall` no longer apply.
>
> **Migration:** if you previously ran `fno watch install`, the launchd job is now orphaned (its plist points at the deleted `scripts/abi-watch.sh`). Remove it once: `launchctl bootout gui/$(id -u)/com.fno.watch.<project> 2>/dev/null; rm -f ~/Library/LaunchAgents/com.fno.watch.<project>.plist` (`<project>` = your `project:` name).

## Onboarding a project to the inbox fleet

Onboarding is a single step: set the project name so peers can address you.

**Set the project name.** Edit `<repo>/.fno/settings.yaml` and add (or confirm) `project: <name>` at the top. The name MUST match what peers will use in `fno mail send --to-project <name>` (example known names: `footnote`, `acme-backend`, `acme-web`, `acme-docs`, `acme-blog`, `marketing`). If the field is missing, every `fno mail` verb errors with `set 'project:' in .fno/settings.yaml or pass --from` (recipient verbs keep `--from`; the send verb is `fno mail send --from-name`).

Once named, peers can `fno mail send --to-project <you>` and you read with `fno mail unread` / drain with `fno mail drain`.

## Setup (one-time per project)

Each project must declare its identity in `.fno/settings.yaml`:

```yaml
project: acme-web
```

Without this field, `fno mail` errors loudly. There is no fallback to cwd basename; the project name must be explicit.

Optional triage settings:

```yaml
config:
  inbox:
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
fno mail ack msg-a4f1b2 [--triaged-into ab-1234abcd]
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

## Draining mail

Read and process unread threads with `fno mail` - no daemon required:

```bash
fno mail unread          # list threads addressed to you past your cursor
fno mail drain --max 10  # process unread non-interrupting kinds and ack each
```

`kind: question` threads are never auto-handled: the drain leaves them unread for a human. Run the drain manually, or let an autonomous worker run it.

## Status

`fno mail status` prints a one-screen health snapshot for the current project. Useful when you want to know "do I have unread mail" without grepping log files.

```bash
fno mail status
```

```
project: footnote
daemon: not_installed
inbox path: /Users/me/your-vault/internal/agents/footnote/inbox
unread: 2
acked_24h: 7
last drain: never
active session: idle
wake signals: 0
errors_24h: 0
```

Add `--json` for machine-readable output. The same eight fields appear as top-level keys in the JSON object.

| Field | Meaning |
|-------|---------|
| `daemon` | Vestigial since the `fno watch` daemon was removed; always `not_installed` |
| `inbox_path` | Absolute path to the project's `inbox/` directory |
| `unread` | Count of unread messages |
| `acked_24h` | Count of `read` messages with timestamp within the last 24 hours |
| `last_drain` | Vestigial since the headless drain daemon was removed; always `never` |
| `active_session` | `idle`, `target_active`, or `interactive_active` (matches the daemon's bypass logic) |
| `wake_signals` | Count of pending files in `<repo>/.fno/wake-signals/` |
| `errors_24h` | Count of `inbox-errors.jsonl` entries with a parseable `ts` within the last 24 hours |

`fno mail status` is informational and does not require the daemon to be loaded - it is safe to run even before step 3 of the [onboarding checklist](#onboarding-a-project-to-the-inbox-fleet).

## Architecture reference

Full architecture and design rationale: see [docs/architecture/cross-project-inbox.md](../architecture/cross-project-inbox.md).

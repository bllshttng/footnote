# Inbox handlers

Megawalk's Step 0 invokes `fno mail drain --json --max 10`. The drain
command dispatches each unread thread to its kind-specific handler. You
do not loop over threads or call per-kind helpers yourself - the heavy
lifting lives in `cli/src/fno/inbox/drain.py`.

## Layout

Each unread thread is a markdown file at
`~/your-vault/internal/agents/{recipient}/inbox/{YYYY-MM-DD}-{slug}.md`.
Replies live in the same file as their root message. A thread is
"unread" when its frontmatter has no `read_at:` field; the drain sets
`read_at:` after dispatch (except `question`, see below).

## Per-kind outcomes (3 kinds, post-2026-05)

| Kind | Action | Result location |
|------|--------|-----------------|
| `heads-up` | LLM triage via `claude -p`. On `create_node`, file a graph node carrying `source_inbox_msg` (root msg-id) and `source_inbox_thread` (thread file path). Mark thread read. | `~/.fno/graph.json` (new node with provenance); thread frontmatter gains `read_at:` |
| `question` | Drop a wake-signal; **leave thread UNREAD** | `<repo>/.fno/wake-signals/wake-{id}.json`; thread stays UNREAD until a human answers |
| `fyi` (default) | Append `inbox_fyi` event to `<repo>/.fno/convo-signals.jsonl`. Mark thread read. | `event: "inbox_fyi"` line; thread frontmatter gains `read_at:` |
| `fyi` with `persist_to_memory: true` | Write a recipient memory file. Mark thread read. | `~/.claude/projects/{me}/memory/auto_inbox_lesson_{thread_id}.md`; thread frontmatter gains `read_at:` |

The `persist_to_memory` frontmatter flag is set when the sender used
`fno mail send --to-project <project> --kind fyi --persist memory ...`;
it preserves the old `lesson` kind's behavior on the new layout.

## Deprecated kinds (rejected at the CLI)

`notification`, `lesson`, `answer`, `complete` are removed. The CLI
exits non-zero with a hint pointing at the replacement:

| Old kind | Replacement |
|----------|-------------|
| `notification` | `--kind fyi` |
| `lesson` | `--kind fyi --persist memory` |
| `answer` | `--kind fyi --reply-to <msg-id>` (or any kind with `--reply-to`) |
| `complete` | `--kind fyi --reply-to <msg-id>` |

## Non-blocking question semantics

`kind: question` does NOT block megawalk. The drain drops a
wake-signal so the next active session (SessionStart hook,
UserPromptSubmit hook) surfaces it to the human, but megawalk itself
proceeds. The unread thread is the receipt that the question is still
pending; the human path acks it after answering.

## Replies

Senders use `--reply-to <msg-id>` (any kind) instead of the removed
`answer` kind. When the recipient already has a thread containing
`<msg-id>`, the reply appends to that thread file. When no such thread
exists, a new thread is created with `replies_to: <msg-id>` in
frontmatter so the cross-thread link is durable.

## --max cap

Default `--max 10`. If you need to process more in one Step 0 pass,
raise the cap. Each call processes up to N unread threads and returns;
remaining threads stay UNREAD for the next iteration's Step 0.

## Empty inbox short-circuit

`fno mail drain --json` returns `[]` when there are no unread threads.
Megawalk proceeds to Step 1 with no further inbox work.

## Stop-hook unread re-check

`hooks/target-stop-hook.sh` runs a structural unread scan before
honoring `status: COMPLETE`. The scan globs the recipient's `inbox/`
for files lacking `read_at:` and either (a) logs an
`unread_inbox_messages` event to `hook-events.jsonl` and notifies
(default), or (b) blocks the COMPLETE transition when
`config.inbox.block_complete_on_unread: true` is set.

## See also

- `cli/src/fno/inbox/store.py` - thread-per-file data layer
- `cli/src/fno/inbox/drain.py` - the dispatch logic
- `cli/src/fno/inbox/unread_scan.py` - hook helper
- `cli/src/fno/wake/signal.py` - wake-signal substrate
- `scripts/migrate-inbox-flat-to-threads.py` - one-shot migration
- `skills/inbox/SKILL.md` - sender-facing skill

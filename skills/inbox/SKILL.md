---
name: inbox
description: "Send and receive cross-project messages between agents in different projects (send via `fno mail send --to-project`, drain/read via `fno mail`). Use when an LLM in one project (e.g. acme-backend) wants another project's agent (e.g. acme-web, acme-docs, acme-blog, marketing) to know about or react to something. Recipient daemons drain mail autonomously; only kind:question reaches a human."
argument-hint: ""
metadata:
  internal: false
---

# Cross-project inbox

The cross-project inbox is the message bus between agents in different
repos. You write to it from one project; an agent in another project
reads it and acts. This skill teaches you the contract.

## What this is (post-2026-05 thread-per-file layout)

Each project has an `inbox/` directory whose location is configurable via
`config.paths.inbox_dir` in settings.yaml. For vault setups Jason's auto-
migrated `paths.inbox_dir: "{vault}/internal/agents/{project}/inbox"`,
resolving to `~/your-vault/internal/agents/<project>/inbox/`. Non-vault setups
default to `~/your-vault/internal/agents/<project>/inbox/` for backward
compatibility (override via env var `FNO_INBOX_ROOT` in tests, or via
`config.paths.inbox_dir` in settings.yaml for production). Each
conversation is one markdown file in that directory:
`{YYYY-MM-DD}-{slug}.md`. Replies append to the same file rather than
creating new ones, so the recipient sees a self-contained thread per
file. Senders write via `fno mail send --to-project`. Recipients drain
unread threads via `fno mail drain`, run manually or by an autonomous
worker.

A thread is "unread" when its frontmatter has no `read_at:` field; the
drain sets `read_at:` after dispatch (except for `question`, which
intentionally stays unread until a human handles it).

The drain dispatches per-kind:

- `heads-up` runs LLM triage and may file a graph node with provenance back to your thread.
- `question` drops a wake-signal and stays unread; only `question` is intended to interrupt a human.
- `fyi` is the catch-all "inform but do not act" kind. With `--persist memory`, the recipient writes a memory file that future sessions recall.

You do not need to know the implementation. You need to pick the right
`--kind` (and optionally `--persist memory` or `--reply-to`) so your
message gets handled the way you want.

## When to send

Use the kind that matches your intent:

- You shipped something another agent should react to. Pick `heads-up`. Their drain will triage and may file work into their backlog.
- You finished a long-running job and want a peer FYI'd, no action expected. Pick `fyi`.
- You hit a cross-project ambiguity that you genuinely cannot answer yourself. Pick `question`. A human will see this.
- You learned something the recipient agent should remember in future sessions. Pick `fyi --persist memory`.
- You are answering a previous message. Pass `--reply-to <msg-id>` (any kind) and your reply appends to the existing thread.

If your message is just gossip ("FYI we ran a build"), prefer skipping
it. The inbox is not a chat log.

## The three kinds

Every message has exactly one `--kind`. The recipient's drain uses the
kind as the dispatch key, so a typo silently becomes dead-letter.

### heads-up

The recipient triages your message via LLM. Triage may decide to
`create_node` (file a graph node), `ignore` (just ack), or
`request_clarification` (which leaves the thread unread so a human
responds via reply).

```bash
fno mail send --to-project acme-web --kind heads-up \
    --body "New region data source live in PR 112; web filters need a region column" \
    --ref-pr 112
```

Recipient autonomously: triage runs, a graph node lands at the
recipient with `source_kind: from_inbox`, `source_project: <you>`,
`source_inbox_msg: msg-<root-id>`, `source_inbox_thread:
{path-to-thread-file}`. Your thread flips to read.

### question

Interrupts work. Recipient's drain drops a wake-signal and
intentionally leaves the thread unread so a human (or active session)
handles it.

```bash
fno mail send --to-project footnote --kind question \
    --body "Should I roll the rotation queue back to 5 swaps before 3 a.m.?"
```

Use sparingly. A `question` is the only kind that stays unread after drain.

### fyi

Inform without action. The recipient's drain marks it read and dismisses
it (or persists it as a memory file when `persist_to_memory: true`).

```bash
fno mail send --to-project acme-docs --kind fyi \
    --body "Docs site rebuild kicked off, ETA 4 minutes"
```

#### fyi with `--persist memory`

A cross-project memory write. The recipient drain writes a memory file
at `~/.claude/projects/<recipient>/memory/auto_inbox_lesson_<thread-id>.md`
with frontmatter that marks it auto-generated and back-references your
thread file.

```bash
fno mail send --to-project acme-backend --kind fyi --persist memory \
    --body "fcntl.flock and filelock 3.x are wire-compatible on macOS; reuse the same lock path."
```

Future conversations in the recipient project recall this without you
re-sending it. `--persist memory` is only valid with `--kind fyi`.

## Replying to a message

`--reply-to <msg-id>` is the universal reply mechanism. It works with
any kind and replaces the removed `answer` kind.

```bash
# fno mail reply resolves the recipient project for you by looking up
# <msg-id> in your own inbox - the convenient way to answer mail you got.
fno mail reply --to msg-a4f1b2 --kind fyi \
    --body "Yes, roll back to 5. The 8-cap was for ab-9728b70b only."

# Or via agents send with an explicit recipient project.
fno mail send --to-project footnote --kind fyi --reply-to msg-a4f1b2 \
    --body "..."
```

If the recipient already has a thread containing `<msg-id>`, your
reply appends to that thread file. If not, a new thread is created
with `replies_to: <msg-id>` in frontmatter so the cross-thread link is
durable.

## Sender command reference

All messaging lives under one namespace, `fno mail` (ab-cee91152): publish,
consume, and reply are all `fno mail` verbs over the jsonl-canon bus log.

- `fno mail send --to-project <project> --kind <kind> --body "..."` - send a new thread to `<project>`'s inbox.
- `fno mail send --to-project <project> --kind <kind> --reply-to <msg-id> --body "..."` - reply to an existing thread (appends).
- `fno mail reply --to <msg-id> --kind <kind> --body "..."` - reply, auto-resolving the recipient from your own inbox.
- `fno mail unread [--name <recipient>] [--json]` - list bus messages addressed to you past your cursor.
- `fno mail ack <msg-id> [--name <recipient>]` - advance your read cursor.

Other verbs (`list`, `triage`, `drain`, `lint`, `status`) are
recipient-side or admin-side. Run `fno mail --help` for the full
surface.

## Deprecated kinds

These four kinds were removed in the 2026-05 redesign. The CLI exits
non-zero with a hint pointing at the replacement:

| Old kind | Use instead |
|----------|-------------|
| `notification` | `--kind fyi` |
| `lesson` | `--kind fyi --persist memory` |
| `answer` | any kind with `--reply-to <msg-id>` |
| `complete` | `--kind fyi --reply-to <msg-id>` |

## Provenance flags

When you send `--kind heads-up`, the recipient's triage may file a
graph node from your thread. The provenance back-reference is
automatic: the node carries `source_kind: from_inbox`, `source_project:
<you>`, `source_inbox_msg: msg-<root-id>`, `source_inbox_thread:
{thread-file-path}`. You do not pass `--source-*` flags yourself.

What you can pass to enrich the message context for the triage LLM:

- `--ref-pr <N>` - your PR number. Triage often uses this in the node title.
- `--ref-node <ab-id>` - a graph node ID in your project the recipient might want to peek at.
- `--ref-gate <name>` - a named gate or milestone, e.g. `release-2026-05`.

A worked example:

```bash
# In acme-backend, you just merged PR 112 that adds a region filter.
fno mail send --to-project acme-web --kind heads-up \
    --body "Backend now exposes region on /api/records; web needs a column" \
    --ref-pr 112 \
    --ref-node ab-1f3c9a2b
```

## What the recipient does

A one-paragraph mental model so you can send freely:

- The recipient drains its inbox with `fno mail drain` (run manually
  or by an autonomous worker), reading each unread thread.
- For each unread thread, the drain dispatches by kind. `heads-up`
  triggers triage and may file a graph node. `question` drops a
  wake-signal and stays unread. `fyi` dismisses OR writes
  a memory file (when `persist_to_memory: true`).
- After dispatch, the thread's frontmatter gains `read_at:` (except
  `question`, which stays unread). The thread file is durable in the
  your vault regardless of whether the daemon is running.
- If the daemon is offline, `megawalk` Step 0 drains the inbox at the
  top of every iteration. Mail is never lost.

## Stop-hook unread re-check

`hooks/target-stop-hook.sh` runs a structural unread scan before
honoring `status: COMPLETE`. It globs the local project's `inbox/` for
files lacking `read_at:`. Default policy is notify-only (logs a
`unread_inbox_messages` event and surfaces a notify). Setting
`config.inbox.block_complete_on_unread: true` makes the hook block
COMPLETE until you drain.

You do not need to call anything; the detector is structural.

## Anti-patterns

- Do not send `kind: heads-up` for FYI-only updates. The triage costs LLM tokens. Use `fyi`.
- Do not send `kind: question` for things you can answer yourself with a `/think` pass. A `question` interrupts a human.
- Do not send to a project that has no `project:` field in its `.fno/settings.yaml`. The recipient will reject it.
- Do not put secrets, credentials, or large payloads in the body. Thread files are checked into the your vault.
- Do not bypass `--reply-to` and use a fresh `send` for replies. Without `--reply-to`, threading breaks and the recipient sees orphan messages instead of a self-contained thread.

## See also

- Architecture, on-disk format, and rotation algorithm: [docs/architecture/cross-project-inbox.md](../../docs/architecture/cross-project-inbox.md).
- Operator onboarding (4-step daemon install) and `fno mail status`: [docs/guides/cross-project-inbox.md](../../docs/guides/cross-project-inbox.md).
- Megawalk Step 0 dispatch table (per-kind handlers): `skills/megawalk/references/inbox-handlers.md`.
- Wake-signal channel (consumed by SessionStart and target stop hook): `cli/src/fno/wake/signal.py`.
- Migration script (one-shot, idempotent): `scripts/migrate-inbox-flat-to-threads.py`.

# Cross-project inbox: fleet substrate

The inbox lets four agent roles (footnote, acme-web, example-pipeline, marketing) communicate across projects with traceable, idempotent message delivery. Megawalk drains each project's inbox at the top of every iteration so cross-project knowledge survives session compaction, project boundaries, and offline windows.

## Surfaces ownership: turning peer detection into a mechanical check

The "surfaces ownership" map below is the substrate that makes
cross-project peer detection mechanical instead of LLM-judged.

Skills that prompt for cross-project messaging (`/think`, `/blueprint`, the
`/target` ship recap) need a way to answer "which peer should hear about
this?" without burning an LLM call per file. Two opt-in maps in
`~/.fno/settings.yaml` make the answer mechanical:

```yaml
config:
  inbox:
    peers:
      acme-web:
        surfaces: [api-client, ui, design-tokens]
      acme-backend:
        surfaces: [api-server, schema, etl, domain-data]
      acme-docs:
        surfaces: [user-docs, api-reference]
    surface_patterns:
      api-client:    ["src/api/**", "src/lib/api-client/**"]
      api-server:    ["api/routes/**", "api/handlers/**"]
      schema:        ["migrations/**", "src/db/schema/**"]
      design-tokens: ["src/styles/tokens/**", "design-system/**"]
      api-reference: ["api/routes/**"]
```

The two maps are intentionally orthogonal. `peers.<name>.surfaces` lists
the *names* a peer claims to own (chosen freely per workspace);
`surface_patterns.<name>` maps each name to the file globs that constitute
that surface. This separation lets a single surface (e.g. `api/routes/**`)
be owned by more than one peer (the backend that implements it AND the
docs project that references it), without forcing a peer's surface list to
explode into raw glob soup.

`fno.inbox.settings.read_peer_surfaces()` and
`read_surface_patterns()` return these maps as plain `dict[str, list[str]]`
values. Both walk up to the nearest `.fno/settings.yaml` (project-
local override → global), and both return `{}` silently when the block is
absent. The opt-in default is "no peer messaging", so a workspace without
this configuration sees no extra prompts and sends no messages.

Skills consume the maps via the same algorithm:

1. Read `peers` and `surface_patterns`.
2. For a candidate trigger (a `/think` unknown, a `/blueprint` Files-to-Modify
   row, a /ship merged-diff entry), find the surface name(s) whose globs
   match.
3. Find the peer(s) whose `surfaces:` list includes those names.
4. If exactly one peer matches: send (or, for /ship, send-if-not-already
   -in-`messaged_peers:`). If multiple peers match the same name: emit a
   `<help reason="cross-project-disambiguation">` and skip rather than
   multi-peer blasting.

The `messaged_peers:` plan-frontmatter field is the dedup substrate. Once
/think or /blueprint sends a message to a peer for a given plan, the peer name
lands in `messaged_peers:` and the /ship recap step skips that peer.

## Two substrates, one feature

The system rides on two storage layers that already existed for related but distinct purposes:

**graph.json** stays the source of truth for *work*. A new graph entry represents a feature that should ship. The plan adds four nullable provenance fields to every entry so the work can carry a "where did this come from" trail:

```yaml
source_kind: organic | from_inbox | from_observation | from_supervisor
source_project: example-pipeline            # who told us about it
source_session_id: 20260504T235919Z-...      # target session that produced the heads-up
source_inbox_msg: msg-a4f1b2                 # the inbox message that triggered triage
```

**`~/your-vault/internal/agents/{project}/inbox/`** (post-2026-05) is the
substrate for *conversation*. Each agent owns a folder; the `inbox/`
directory inside it holds one markdown file per thread, named
`{YYYY-MM-DD}-{slug}.md`. Replies append to the same thread file rather
than creating new files, so a recipient sees a self-contained
conversation per file instead of a wall of unrelated msg-blocks.

Pre-2026-05 layouts used a single flat `inbox.md` per recipient. The
migration script `scripts/migrate-inbox-flat-to-threads.py` is a
one-shot, idempotent rewrite that splits flat files into thread files
and renames the original to `inbox-pre-migration.md` as a safety net.

The two substrates never overlap: questions/fyi never become graph
entries; only triaged heads-up threads do (via `fno mail triage` ->
`fno new --source-*`).

## Symmetric mailbox model

```
        example-pipeline                      acme-web
                |                                    |
                |  fno mail send                   |
                |     --to-project acme-web -------> | append to web.md
                |     --kind heads-up                |
                |                                    | fno mail unread
                |                                    |   sees new msg
                |                                    | fno mail triage <id>
                |                                    |   -> claude -p
                |                                    | fno new --source-* ...
                |                                    | fno mail ack <id>
                |                                    |
                |  fno mail send                   |
                |     --to-project example-pipeline  |
                |     --kind question <-------------- |
                |     (interrupts mid-feature)       |
```

Sender resolution: `resolve_project()` walks up from cwd looking for `.fno/settings.yaml` with a `project:` field. Without that field, every `fno mail` verb errors with the exact fix string `"set 'project:' in .fno/settings.yaml or pass --from"` (the reply/drain verbs keep `--from`; the cursor `unread`/`ack` take `--name`; the send verb `fno mail send` uses `--from-name`).

## Three message kinds (post-2026-05)

| Kind | Action | Mid-feature? |
|---|---|---|
| `question` | drop a wake-signal, leave thread unread so a human handles it | YES (interrupts) |
| `heads-up` | call `fno mail triage <msg-id>`; dispatch on action plan; mark thread read | NO (between features) |
| `fyi` | log a line to `.fno/convo-signals.jsonl`, mark thread read | NO (between features) |
| `fyi --persist memory` | write a recipient memory file with `auto_generated: true`, mark thread read | NO (between features) |

Only `kind: question` interrupts an in-flight feature. Everything else
waits between features so cross-project chatter cannot derail focused
work. Replies use `--reply-to <msg-id>` (any kind) instead of the
removed `answer` kind: `--reply-to` appends to the existing thread file
when the recipient already has it, otherwise creates a new thread with
`replies_to: <msg-id>` in frontmatter.

The pre-2026-05 layout had five kinds (`question`, `answer`, `heads-up`,
`notification`, `lesson`). The CLI now rejects the four removed kinds
with a hint pointing at the replacement. The migration script
collapses old kinds onto the new vocabulary (notification -> fyi,
answer -> fyi-with-replies-to, lesson -> fyi-with-persist_to_memory,
complete -> fyi-with-replies-to).

## Concurrency: per-thread mkdir mutex

Concurrent writers to the same thread file serialize via a
`mkdir <path>.lock.d` mutex (POSIX-atomic, macOS portable). Read-only
verbs (`unread`, `list`, `find_thread_by_msg_id`) skip the lock. Writes
go through `_atomic_write_text(path, content)`: write to a sibling
tempfile (mkstemp), then `os.replace(tmp, target)` so a partial write
under SIGKILL or disk-full cannot leave the live thread file
truncated. Filename allocation in `write_new_thread` uses
`O_CREAT|O_EXCL` so two senders racing on the same `{date}-{slug}.md`
both get distinct files.

## Archive rotation

`fno.inbox.archive.archive_old_threads(recipient, settings)` moves
stale read threads from `{recipient}/inbox/` into
`{recipient}/inbox/archive/{YYYY-MM}/`. The month derives from the
thread's `read_at` (or `created` when read_at is somehow missing).
Sorting is by `read_at` descending, so the most recent
`settings.keep_recent_read` (default 50) read threads stay in the live
folder and the rest are archived. Unread threads (frontmatter has no
`read_at:`) are NEVER archived, regardless of count or size.

The auto-rotate-on-write path from the old flat-file layout is
removed; rotation is now an explicit operator action (manual
invocation or a periodic launchd task).

## LLM triage seam

`fno mail triage <msg-id>` shells out to `claude -p` with a structured prompt and JSON schema, returning a typed `TriagePlan`:

```python
@dataclass
class TriagePlan:
    action: Literal["create_node", "ignore", "request_clarification"]
    title: str | None        # required when action == create_node
    priority: str | None     # p0..p3, required when action == create_node
    body: str
    follow_up_question: str | None  # required when action == request_clarification
```

The subprocess respects `FNO_INBOX_TRIAGE_STUB` so tests can inject canned plans without burning real LLM tokens. On parse or schema failure, retry-once with a stricter reminder; on second failure, log to `.fno/inbox-errors.jsonl` and raise `TriageFailedError` (the message stays unread for the next iteration).

## Idempotent triage (crash recovery)

If the drain dispatcher creates a graph node but crashes before acking the inbox message, the next iteration would re-triage and create a duplicate. The dedup key is `source_inbox_msg`:

```python
existing = query_by_source_inbox_msg(msg_id)
if existing:
    fno mail ack <msg-id> --triaged-into existing[0]['id']
    # skip triage entirely
else:
    fno mail triage <msg-id>
    # ... dispatch on plan
```

Scenario A's crash-recovery sub-test exercises this end-to-end.

## Megawalk integration

A new Step 0 at the top of every megawalk iteration drains the inbox. The reference doc at `skills/megawalk/references/inbox-handlers.md` spells out the per-kind handler logic, the cap (10 messages per iteration), and the empty-inbox short-circuit (one fast `unread --json` filesystem read).

## Headless drain + wake-signal channel

Megawalk's Step 0 only runs at the top of a target iteration. Projects without an active session would otherwise leave unread inbox messages sitting indefinitely. The headless drain and its companion daemon close that gap: a per-project background watcher ensures mail gets processed even when no human is at the keyboard.

### The three pieces

- **`fno mail drain --json --max 10`** - an LLM-side processor for the four non-interrupting message kinds: `heads-up`, `notification`, `lesson`, and `answer`. For each unread message it runs the appropriate handler (triage to `fno new`, write to convo-signals, extract to memory, integrate into context) and acks. A `kind: question` message is never handled here: the drain drops a wake-signal and leaves the message unread for a human to answer.

- **Per-project launchd daemon** (`scripts/abi-watch.sh`, opt-in via `config.inbox.watch.enabled: true`) - wraps the drain with an `fswatch` trigger loop. On each file-change event it calls `wake.detect.detect_session_state` to determine whether a human or target session is already active. When the project is IDLE it spawns `claude -p --bare` to run the drain. When a session is active it either drops a wake-signal (INTERACTIVE_ACTIVE) or does nothing at all (TARGET_ACTIVE, because megawalk Step 0 will pick the message up at the next iteration boundary).

- **Wake-signal channel** at `<repo>/.fno/wake-signals/wake-{id}.json` - one JSON file per signal, written atomically. The daemon creates it; three readers consume it: the SessionStart hook, the UserPromptSubmit hook, and the target stop hook. The stop hook is log-only - it records the pending signal but does not refuse exit. The session hooks prepend a system reminder to the next LLM turn and delete the file.

### Wake-signal envelope

```json
{
  "signal_id": "wake-{8 hex}",
  "source": "inbox-drain",
  "kind": "question",
  "msg_id": "msg-a4f1b2",
  "from": "example-pipeline",
  "summary": "blocking on the record-parser shape decision",
  "ts": "2026-05-05T17:14:00Z"
}
```

The `kind` field mirrors the inbox message kind. Currently only `kind: question` signals are written, because question is the only kind that requires a human response. The `summary` is a one-line digest the daemon extracts from the message body so the session hook can surface it without re-reading the inbox file.

### Three execution modes

**IDLE target project.** The fswatch event fires; `detect_session_state` returns `IDLE`; the daemon spawns `claude -p --bare` with the drain prompt. The LLM processes every unread message, acks each one, and exits. The daemon resumes watching.

**INTERACTIVE_ACTIVE target project.** The user has an open Claude Code session (a recent transcript jsonl exists). The daemon drops a wake-signal at `.fno/wake-signals/wake-{id}.json` and exits without spawning a subprocess. The next time the user submits a prompt, the UserPromptSubmit hook reads the signal files, prepends "Inbox: 1 question from {project}" to the prompt as a system reminder, and deletes the signal. The session LLM sees the question and can answer it inline.

**TARGET_ACTIVE target project.** A target session is in progress (`target-state.md` is `IN_PROGRESS` and recently modified). The daemon does nothing: spawning a second `claude -p` process would race with the in-flight session. Megawalk's Step 0 at the next iteration boundary will drain the inbox in the normal flow.

### `kind: question` is the human escape hatch

By design, only `kind: question` ever reaches a human. The daemon and the drain LLM process all four other kinds autonomously. Questions are explicitly excluded from autonomous handling because they represent a cross-project decision that requires the recipient's judgment. Dropping a wake-signal is the daemon's acknowledgment that a human needs to see this; it never attempts to answer on the human's behalf.

### Cross-references

- `docs/guides/cross-project-inbox.md` - operator-facing install and uninstall instructions for the daemon
- `cli/src/fno/inbox/drain.py` - drain command dispatch logic and `--headless` flag
- `cli/src/fno/wake/signal.py` - wake-signal write, read, and delete substrate

## Out of scope (follow-ups)

The substrate ships as a single feature. Four follow-up backlog entries land in `cli/FOLLOW-UPS.md`:

- Supervisor sweep (`fno supervise`) - cron-driven anomaly detection that uses the inbox to question and lesson back to projects
- Daily brief generator - 9am/5pm rollups of overnight work + unread inbox + personal TODOs
- Headless `fno watch` daemon - launchd-managed per-project poller
- Marketing role automation - Gmail outreach + content schedule via inbox heads-ups

## Files

- `cli/src/fno/inbox/store.py` - Markdown parser, `Message`/`Kind`/`Status`, filelock, monotonic transitions, `resolve_project()`
- `cli/src/fno/inbox/cli.py` - Six Typer verbs (send/unread/ack/reply/list/lint) + triage subcommand registration
- `cli/src/fno/inbox/archive.py` - `needs_rotation`, `rotate`, `read_inbox_settings`
- `cli/src/fno/inbox/triage.py` - `claude -p` subprocess wrapper with retry-once and env-stub override
- `cli/src/fno/graph/store.py` - Provenance setdefaults in `_apply_graph_defaults`
- `cli/src/fno/graph/load.py` - `query_by_source_inbox_msg` helper
- `cli/src/fno/graph/cli.py` - `--source-*` flags on `fno new`
- `skills/megawalk/SKILL.md` - Step 0 drain section
- `skills/megawalk/references/inbox-handlers.md` - Per-kind handler reference


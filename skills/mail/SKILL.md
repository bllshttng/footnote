---
name: mail
description: "Message background agent workers and projects from a runner-less surface (phone / Happy app). One front door over the shipped `fno mail` messaging surface (live-inject first, durable queue as the fallback): send (message a peer or a project), reply (correlated response), unread / list / view / status (read your inbox), ack (advance your read cursor), drain (batch-consume at a loop boundary). Normalizes messy input (smart quotes, recipient, body), refuses an empty recipient or body before writing anything, runs the genuine `fno mail` command, and reports the real msg-id receipt - never a fabricated one. Messaging is free, so it never confirms - but it never reports a durable queue as delivered either. Use when: 'send tgt-foo a message', 'mail target about X', 'check my unread', 'reply to msg-abc', 'tell everyone on project Y', 'what's in my inbox'."
argument-hint: "<verb> [args]  |  send <name> \"<body>\"  |  reply <msg-id> \"<body>\"  |  unread|list|status [name]"
metadata:
  internal: false
requires:
  binaries:
    - "fno >= 0.1"
---

# Mail

**Message background workers and projects from anywhere - even your phone.**

`/fno:mail` is the runner-less front door over the shipped `fno mail`
messaging surface. `fno mail send|reply|unread|...` needs exact shell quoting, and
a phone has no `!` local-command runner, so a typed command either splits on bad
smart quotes or never executes at all. This skill fixes that the same way
`/agent` does: **you (the agent) are the runner.** You read the messy input,
route it to the right `fno mail` verb, normalize it, run the **genuine** command
via your Bash tool, and report the **real** captured receipt.

`/agent` is agent **lifecycle** (spawn / watch / stop). `/mail` is **messaging**.
Two skills, two concerns. This skill REUSES the shipped `fno mail` primitives -
it does not reimplement the bus, the cursor, or the render. Its value is verb
routing + input normalization + honest reporting for surfaces that cannot run a
local command.

`SKILL_DIR` below is `skills/mail` inside this plugin.

## The delivery model (so the verbs make sense)

**A send injects into the recipient's live session. The durable queue is what
happens when that misses, and it is recovery, not delivery.**

`fno mail send` tries a live inject first. On success nothing is written to the
bus at all: the `<fno_mail>` turn lands in the recipient's session and it acts on
it this turn. Only when there is no live inject path does the envelope fall to
the durable `messages.jsonl` log, where it waits on a drain the recipient may
never run. Both exit 0, so **read the receipt, not the exit code**:

- `msg-<id> delivered (hosted)` - it landed. This is the normal outcome.
- `msg-<id> queued (durable)` - **not delivered.** Nobody checks their voicemail.

The recipient's own `unread` / `ack` / `drain-self` verbs exist to consume that
fallback queue, which is why they read like a mailbox. They are the recovery
path, not the main one.

### A miss is recoverable now - do not wait

A durable receipt usually means the session is idle rather than gone, and an
idle session can be brought back. There is no reason to wait on a drain when you
can get an answer immediately:

```bash
fno agents peek <short-id>     # read-only: is it actually alive?
fno agents resume <short-id>   # idle -> live, then re-send
fno agents attach <short-id>   # drive it yourself (claude)
```

The address you send to IS the id these take: one bare 8-hex short-id keys mail,
resume, attach, peek, and the transcript. If a send missed, the same string gets
you in.

## Verb router

The first whitespace token of the argument is the **verb**. Route on it, then run
the matching section. Messaging is free, so **nothing here confirms** (contrast
`/agent`, where `chat`/`stop` confirm).

| Verb | Routes to | Needs normalize | Cost |
|------|-----------|-----------------|------|
| `send <name> "<body>"` | `fno mail send <name> "<body>"` | yes (refuse empty) | free |
| `send project <X> "<body>"` | `fno mail send --to-project <X> "<body>"` | yes (broadcast) | free |
| `reply <msg-id> "<body>"` | `fno mail reply --to <msg-id> --body "<body>"` | yes (refuse empty) | free |
| `unread [name]` | `fno mail unread [-n <name>]` | no (read) | free |
| `ack <msg-id> [name]` | `fno mail ack <msg-id> [-n <name>]` | no | free |
| `list` | `fno mail list` | no (read) | free |
| `view` | `fno mail view` | no (read) | free |
| `status` | `fno mail status` | no (read) | free |
| `drain` | `fno mail drain` | no | free |

An unrecognized leading token is an error - tell the user the verb set above; do
NOT guess a send. (Unlike `/agent`, a bare non-verb is not a default action here,
because a misrouted `send` could publish a malformed message.)

---

## `send <name> "<body>"` - publish to a peer or a project

The core verb (US4). Strip the leading `send`; the rest is `<recipient> <body>`
(name mode) or `project <X> <body>` / `--to-project <X> <body>` (broadcast mode).

### Flow: NORMALIZE -> RUN -> REPORT (no confirm: free lane)

#### 1. NORMALIZE (deterministic helper)

Run the normalizer with everything after the `send` verb as ONE `--input`:

```bash
bash "${SKILL_DIR}/scripts/normalize.sh" --verb send --input "<recipient/project + body>"
```

It strips smart quotes, splits the recipient (or detects a `project`/`to-project`
broadcast keyword) from the body, strips one wrapping quote pair off the body,
and **refuses an empty or whitespace-only recipient or body**. Read its
`key=value` output; never `eval` it. Fields: `status`, `error`, `verb`,
`recipient`, `to_project`, `body`. `body` is emitted **last** and may span
multiple lines (a pasted multiline message is preserved intact), so capture
**everything after the `body=` marker** to end of output as the body - do not
truncate it at the first newline.

- If `status=error`, **STOP. Report the `error=` line and run nothing** (AC4-ERR).
- Otherwise capture `recipient` / `to_project` / `body`.

#### 2. RUN (genuine execution)

Run the real `fno mail send` - never reimplement the bus. Pass the body as a
single quoted argument exactly as normalize returned it:

```bash
# name mode (recipient non-empty):
fno mail send "<recipient>" "<body>"

# broadcast mode (to_project non-empty):
fno mail send --to-project "<to_project>" "<body>"
```

#### 3. REPORT (echo ONLY what actually happened)

`fno mail send` exits 0 for both outcomes, so the receipt line is the only signal:

- `msg-<id> delivered (hosted)` - a live recipient took it now. Report it plainly.
- `msg-<id> queued (durable)` - **report this as NOT delivered**, not as success.
  The CLI prints the recovery ladder on stderr; relay it. Offer to `resume` the
  session and re-send rather than telling the user to wait for a drain.

Relay the **real** msg-id and the resolved recipient/project so delivery is
auditable. Never round a durable receipt up to "sent" - that is the phrasing that
lets a message sit unread for a day while both sides believe it arrived.

#### Send-and-hold: make your reply reachable

A name-lane send (`send <name>`) already self-stamps the sender: omit `--from-name`
and the envelope carries your canonical reply handle, so a reply resolves. A
`--to-project` send instead stamps the *project* as sender by default (correct for
fire-and-forget notes, wrong when you hold for an answer). If you send
`--to-project` and will wait for the reply, pass `--from-self` - it stamps your own
reply handle (exit 2 if there is no ambient harness identity, never a silent
floor). The `mail:` line of `fno whoami` is the only identity field that is a valid
`--from-name`; never copy the `run:` line (a ledger id, not a handle).

- **Unknown name** (`fno mail send` exits 16): report "unknown agent `<name>` -
  nothing was written" and do NOT guess a recipient.
- **Broadcast ambiguity** (multiple live peers for a project, exit nonzero): relay
  the candidate list `fno mail send` printed; suggest `--any` only if the user
  meant "any one of them".
- Any other nonzero exit: report **FAILED** with the captured stderr. NEVER report
  a phantom delivery or a fabricated msg-id.

---

## `reply <msg-id> "<body>"` - correlated response

Strip the leading `reply`; the rest is `<msg-id> <body>`.

### Flow: NORMALIZE -> RUN -> REPORT (no confirm)

1. **NORMALIZE.** `bash "${SKILL_DIR}/scripts/normalize.sh" --verb reply --input "<msg-id + body>"`.
   It refuses an empty msg-id or empty body. On `status=error`, STOP and report the
   `error=` line. Capture `msg_id` and `body`.
2. **RUN.** `fno mail reply` takes the id and body as **flags** (not positionals):

   ```bash
   fno mail reply --to "<msg_id>" --body "<body>"
   ```

3. **REPORT.** Relay the real outcome (`fno mail reply` correlates the thread via
   `in_reply_to`). On a nonzero exit, report FAILED with the captured stderr -
   never a phantom reply.

---

## `unread [name]` / `list` / `view` / `status` - read your inbox (thin pass-through)

Reads. No normalize, no confirm. Run the raw verb and relay its output faithfully.

```bash
fno mail unread                 # my default inbox (project 'footnote')
fno mail unread -n "<name>"     # a specific agent/project inbox
fno mail list                   # unread threads in my inbox (-A for all)
fno mail view                   # render the bus log as an inbox view
fno mail status                 # one-screen inbox health snapshot
```

`unread`/`ack` take the inbox name as the `-n/--name` **option** (default
`footnote`), NOT a positional - pass `-n "<name>"` when the user names a specific
inbox. If the raw verb errors, relay the real error; do not invent output.

---

## `ack <msg-id> [name]` - advance your read cursor

`ack` marks everything up through `<msg-id>` seen. The msg-id is positional and
required; the inbox name is the `-n` option (default `footnote`).

```bash
fno mail ack "<msg-id>"               # advance my default cursor
fno mail ack "<msg-id>" -n "<name>"   # advance a specific inbox's cursor
```

Refuse an empty msg-id before running. Relay the real outcome. The advance is
idempotent (re-acking the same id is a safe no-op), so a retry never double-skips.

---

## `drain` - batch-consume at a loop boundary

Drains unread threads with per-kind dispatch (heads-up -> triage; question ->
wake-signal; fyi -> log/memory). Run it raw and relay the summary:

```bash
fno mail drain            # default cap of 10 threads
fno mail drain --max 25   # raise the per-call cap
```

---

## Hard rules (non-negotiable)

1. **Never fabricate a receipt.** Report ONLY a msg-id / outcome line that
   `fno mail` actually printed. "No receipt" is FAILED, full stop. This is the
   cardinal guard (same as `/agent`).
2. **Refuse empty input.** An empty recipient, empty body, or empty msg-id is a
   refusal **before any command runs** (AC4-ERR) - the normalizer enforces it for
   `send`/`reply`; you enforce it for `ack`.
3. **Never confirm.** Messaging is free; there is nothing billed or destructive
   to gate. Sending without asking is fine - reporting a durable queue as
   delivered is not (rule 4).
4. **A durable receipt is not delivery.** Say so, and offer the recovery ladder
   (`peek` / `resume` + re-send / `attach`). Waiting on a drain is the last
   resort, never the advice you lead with.
5. **Do not reinvent the bus.** Addressed delivery, the cursor, the render, and
   rotation all live in `fno mail`. This skill routes verbs, normalizes input, and
   reports honestly; it never duplicates that machinery.
6. **Use the genuine CLI shapes.** `send <name> <body>` and `--to-project <X>` are
   positional/flag; `reply` is `--to <id> --body <text>`; `unread`/`ack` name is
   `-n`. Do not pass a body as a positional to `reply`.

## Multi-CLI

This skill is Claude-Code primary but provider-neutral: it only needs the `fno`
binary (the `fno mail` mailbox is provider-agnostic - claude, codex, and gemini
peers all read and write the same bus). On a CLI without `fno`, the command fails
loud and nothing is written - it degrades honestly, never fakes a delivery. See
[docs/SKILL-COMPAT-MATRIX.md](../../docs/SKILL-COMPAT-MATRIX.md).

To dispatch or observe a worker (rather than message one), use `/agent`.

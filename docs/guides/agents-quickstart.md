# Agents quickstart: spawn and message peers

footnote can launch a worker agent on Claude, Codex, or Gemini and coordinate with it over a message bus. The two verbs you start with are `spawn` (create a peer) and `ask` (message one that exists). This is the short version; the deep guides are linked at the end.

## Prerequisites

- The provider CLI you want (`claude`, `codex`, or `gemini`) on `$PATH` and signed in.
- The `fno` CLI installed (the compiled `fno-agents` binary ships with it).

## Spawn a peer

```bash
fno agents spawn reviewer "review the diff on this branch" -H codex
```

`spawn <name> "<initial message>" -H <harness>` creates a named, persistent peer and hands it the first message. A Claude peer runs as a `claude --bg` thread; a Codex or Gemini peer runs as a PTY-backed worker under the `fno-agents` daemon. You get back one JSON receipt line carrying the peer's `short_id`:

```json
{"name": "reviewer", "short_id": "7c5dcf5d", "provider": "codex", "status": "live"}
```

Pipe it: `fno agents spawn w1 "task" -H claude | jq -r .short_id`.

## Message a peer that exists

```bash
fno agents ask reviewer "what did you find?"
```

`ask <name> "<message>"` delivers to the running session and prints the recipient's reply on stdout, verbatim, no banner or wrapper. The peer keeps working on its own loop between messages. Asking a name that doesn't exist errors with exit 16 ("spawn it first"); creation and messaging are deliberately separate verbs.

## Ask another model a one-off question

When you just want one answer from another model and no lingering peer, spawn an ephemeral worker:

```bash
fno agents spawn q "summarize the failing tests" -H codex --once
```

`--once` (Codex and Gemini) creates the worker, exchanges one round, and tears it down. The model's reply is stdout (the deliverable); the teardown receipt rides stderr; no registry row survives. `--once` with Claude is refused, because Claude peers are persistent background threads; use a plain `spawn` there.

## Observe and tear down

```bash
fno agents list                 # registered agents and their status
fno agents logs reviewer        # tail a peer's output
fno agents stop reviewer        # stop the underlying session
fno agents rm reviewer          # remove the registry row
```

## Where this goes

Each agent runs its own loop and they coordinate over the bus, so you can put a Claude builder and a Codex reviewer on the same repo and let them work in parallel. From a phone or any runner-less surface, the `/fno:agent` skill is a friendlier router over these same verbs (it normalizes messy input and confirms a billed launch before it happens).

## See also

- [fno agents spawn](fno-agents-spawn.md) - every spawn flag (`--once`, `--fresh`, `--here`, `--cwd`), receipts, and exit codes
- [fno agents ask follow-up](fno-agents-ask-followup.md) - the messaging half: `--from-name`, reply waits, lock behavior
- [Cross-project inbox](cross-project-inbox.md) - messaging between whole projects with `fno mail`

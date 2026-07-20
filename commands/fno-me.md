---
name: fno-me
description: Join this session to the fno mesh so other sessions can message you
---

# fno-me

Register THIS session in the fno roster so peer sessions can discover and message
it. A session a human started by hand (not one `fno agents spawn` created) has no
roster row until you run this. Afterward a peer reaches you with
`fno mail send <your-handle>` - live if you are running, durable-queued and
drained at your next SessionStart if not.

Run it, then report the resulting handle back to the user:

```bash
fno agents register
```

It resolves this session's ambient harness identity and writes an `idle` row named
by the canonical bare short-id handle (e.g. `3ad1f42d`) - the same string this
session self-stamps and drains, and the same one `resume` / `attach` / `peek`
take, so delivery is coherent and a missed send is recoverable by the same id. Idempotent
(safe to re-run) and needs no arguments; pass `--name <name>` only to override the
derived handle. Exit 3 means the session has no addressable harness identity
(nothing to register) - report that rather than inventing a handle.

This joins only THIS session. To auto-join every hand-started session in scope,
set `agents.auto_register_sessions = true` in config; the default (`false`) is
opt-in, which is exactly this command.

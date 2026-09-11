# `fno agents peek` design

`peek` is the read-only twin of `fno agents mail send`.

Peek output is NOT a liveness signal. A transcript is a file, and a dead session's file still reads fine. Recent-looking content here proves only that bytes were written at some point, never that anyone is home now. This was misread as proof of life for a session that had been dead 43 minutes, alongside two other surfaces saying the same wrong thing. For a reachability verdict use `fno agents truth <handle>` (or the `reachability` field on `fno agents list`), which is the one derivation with a declared basis and the falsifiers applied. See `fno.agents.reachability`.

Reply is agent-native: `mail send` resolves `<handle>` across every live source. Observe was tribal knowledge: `agents logs` is registry-only, and a live codex thread or an unrostered `claude --bg` session had no single observe verb. `peek` closes the asymmetry: the same union resolver as send for transcript-backed peers, plus a mux-pane arm for pane-substrate workers (the default substrate), whose content is a PTY rather than a transcript.

Two data paths, tried in order:

1. Status stream (fast-path, opportunistic). The normalized `task_started` / `task_done` / `blocked` / `run_summary` events a worker emits to `events.jsonl`. Cheap and cross-harness. Not shipped by every worker yet, so absent falls through with no error.
2. Transcript tail (fallback, ships now). Resolve the handle to the harness's on-disk transcript and tail the last N records. Works for every worker today.

The per-harness on-disk shape differs: claude and codex write one JSONL; opencode writes a per-message dir joined against a per-message parts dir. The extensible seam is `recent_records`, dispatching on `agent`.

Read-only invariant: peek opens files for read and polls stat for `--follow`. It never writes `events.jsonl`, the peer transcript, the registry, or a mailbox. Observing must not perturb the observed.

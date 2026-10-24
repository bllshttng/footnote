# Launch a Muse Code worker

Muse Code has no pane lane in fno. A worker runs on one of two lanes, and neither uses a PTY.

A teammate that takes mail runs on the thread lane. An fno host holds one `muse serve` session over MSP:

```bash
fno agents spawn -H muse --as thread --name <name> --node <node> 'Work node <node>; commit and report.' -- --yolo
```

A one-shot turn runs on the headless lane, `muse exec --json`:

```bash
fno agents spawn -H muse --as headless --name <name> 'Summarize the open PRs.'
```

Every token after `--` reaches `muse serve` or `muse exec` unchanged. Use it for `--provider`, `--model` or `--preset`. Muse refuses a flag it does not know.

## The payload

Write the task as plain text. A message that starts with `/<skill>` goes to Muse as a skill part, and Muse resolves the skill by its bare name. In this repo Muse loads fno's skills from `.agents/skills/`. A checkout without that projection has no fno skills, so a `/target` payload fails there with `skillNotFound`. fno installs no Muse stop hook yet, so a looping `/target` dispatch refuses.

## The sandbox

The default Muse sandbox refuses writes outside the workspace, `~/.fno` included. A worker that must run `fno backlog` or send `fno agents mail` needs `-- --yolo` after the fence. On the thread lane fno maps `--yolo` to `muse serve --disable-sandbox --trust-workspace` and approval mode `allowAll`.

## Get the result back

- `fno agents mail ask <name> "<text>"` submits a turn and prints the delivery state.
- `fno agents mail ask <name> --answer allow_once|deny` decides a pending approval.
- `fno agents session stop <name>` interrupts the turn and leaves the session.
- `muse resume <session-id>`, run from the session's own cwd, opens it in the TUI once no host holds it.

## Known refusals

- `sessionInUse`: another host, such as a Muse TUI, holds the session.
- An unmeasured substrate, such as `--as pane`. Spawn names it and stops.

Back to [the harness choice table](../SKILL.md#launch-a-worker).

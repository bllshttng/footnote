# footnote's opencode plugin

footnote's self-contained opencode orchestration layer. footnote ships its own
native opencode plugin instead of depending on an external orchestration package
for task delegation, identity, and agent registration.

## What's here

| Path | What it is |
|---|---|
| `plugins/fno.ts` | The plugin. opencode auto-scans `.opencode/plugins/*.ts` and loads it directly — no build step. |
| `fno-orchestrator.md` | The orchestrator system prompt injected at session start. |
| `agents/{explore,oracle,librarian}.md` | Three native opencode agents, auto-loaded from `.opencode/agents/`. |
| `tests/fno.test.ts` | `bun test` unit coverage for the pure helpers + task tool. |

## What it does (and what opencode does natively)

The plugin only supplies what opencode can't infer on its own:

- **`config` hook** — registers footnote's existing `agents/*.md` (translated to
  opencode's agent shape) so `task({ subagent_type: "fno:archer" })` resolves.
- **`experimental.chat.system.transform`** — injects the orchestrator identity.
- **`task` / `task_result` tools** — delegation. `task` creates a child session
  and returns its result synchronously (via a blocking `session.prompt`), or a
  `task_id` when `run_in_background: true` (via `promptAsync`); `task_result`
  fetches a backgrounded result. Guards: depth 3, 5 concurrent sync
  delegations, 120s sync timeout, empty-output detection.

opencode does the rest **natively** — it auto-loads `.opencode/agents/*.md`,
discovers `skills/**/SKILL.md` (footnote's skills already ship in that shape),
and exposes its own `skill` tool. That's why there is no custom skill tool,
no build toolchain, and no vendored agent framework here.

## Activation is opt-in

The plugin auto-loads but stays **inert** until you opt in, so opening this repo
in opencode while another orchestration plugin is still active never collides on
the `task` tool. Activate for a session:

```bash
FNO_OPENCODE=1 opencode
```

With `FNO_OPENCODE` unset, the plugin registers nothing.

## Full cutover (make fno the sole orchestration plugin)

When you're ready to make fno the sole orchestration plugin, edit your global
`~/.config/opencode/opencode.json` and drop any other orchestration plugin entry
from the `plugin` array. footnote's plugin auto-loads from this repo's
`.opencode/plugins/` for sessions in this project; for other projects, add a
`file:` entry pointing at `plugins/fno.ts` or publish the plugin to npm. Once no
other orchestration plugin is loaded you can also run without the `FNO_OPENCODE`
gate if you edit `isActivated` to default on. This is a local-machine change and
is deliberately not automated.

## Model routing

Category -> model routing is best-effort and off by default: `CATEGORY_MODEL` in
`plugins/fno.ts` is empty, so delegation rides each agent's own `model:` field
plus opencode's default. To force a model per category, add entries (e.g.
`ship: "anthropic/claude-haiku-4-5"`); the plugin only applies one when the
provider registry actually has it, and otherwise falls back silently. A
`.fno/config.toml [opencode]` override surface is a deliberate follow-up, not v1.

## Tests

```bash
cd .opencode && bun install && bun test
```

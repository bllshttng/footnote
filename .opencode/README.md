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
| `skills/` | Symlink farm: `<name> -> ../../skills/<name>` for every shipped skill. opencode scans `.opencode/skills/`, never the repo-root `skills/`, so these tracked links are what makes footnote's skills discoverable on a fresh clone (and in any worktree - the links are relative). |
| `tests/fno.test.ts` | `bun test` unit coverage for the pure helpers + task tool. |

The repository used to carry its own `.opencode/commands/` directory: five
bare-named stubs (`target`, `think`, `review`, `fix`, `pr`) that existed in no
other project and disagreed with every renderer (`/fno:target` was asked for,
`target` was what existed). The global install now generates the correct
`fno:<verb>.md` names everywhere, this repository included.

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
discovers skills through the `.opencode/skills/` farm (its scan paths are
`.opencode/skills/`, `.claude/skills/`, `.agents/skills/` — the repo-root
`skills/` dir is NOT scanned, which is why the farm exists), and exposes its
own `skill` tool. That's why there is no custom skill tool, no build toolchain,
and no vendored agent framework here.

## Activation is opt-in

The plugin auto-loads but stays **inert** until you opt in, so opening this repo
in opencode while another orchestration plugin is still active never collides on
the `task` tool. Activate for a session:

```bash
FNO_OPENCODE=1 opencode
```

With `FNO_OPENCODE` unset, the plugin registers nothing.

## Global install (every project)

The command, agent and skill catalogs come from one supported install, not from
this repository: `fno config plugin install opencode` writes generated
`fno:<verb>` commands, `fno:<name>` agents, the skill trees, and the stop
bridge into `~/.config/opencode/` (or `$OPENCODE_CONFIG_DIR`), records every
path in a manifest, and refuses to overwrite a file footnote did not write.
Uninstall is `fno-agents plugin-install opencode --uninstall`; it removes only
manifest paths whose bytes still match, keeps and names anything you edited,
and exits 3 when the uninstall was partial. `fno doctor` reports what is
installed versus what the catalogs actually load, by name.

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

Note for Windows: the farm relies on git-tracked symlinks, which need WSL2
(footnote's supported Windows path) or developer-mode symlink support.

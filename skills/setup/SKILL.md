---
name: setup
description: "Schema-driven setup wizard for footnote settings. Derives its question set from the Pydantic config model (the single source of truth) instead of a hardcoded list; asks only the real per-project decisions and defaults the rest."
---

# Setup Wizard

Creates / updates `settings.yaml` by asking only the handful of decisions that
genuinely vary per project, then writing each answer through the validated
`fno config set` path. Everything else is defaulted by the model.

**Single source of truth.** This skill does NOT hardcode a question
list. It asks `fno` for the questions, so the wizard can never drift from the
schema and can never write a key that does not exist:

```bash
fno setup plan              # the ~4-6 "always" decisions, as JSON
fno setup plan --advanced   # also the "advanced" tier
```

Each emitted field carries `{path, type, default, tier, question, default_source, doc}`.

## Modes

| Command | Scope | Fields asked |
|---------|-------|--------------|
| `/setup` | global (`~/.fno/settings.yaml`) | the `always` tier (the real decisions) |
| `/setup advanced` | global | `always` + `advanced` (progressive disclosure) |
| `/setup local` | project (`.fno/settings.yaml`) | same questions, written project-scoped |

There is no separate "full" mode and no hand-maintained question table: `advanced`
is just "ask the advanced tier too". Keys whose tier is `never` are always
defaulted and never surfaced.

## Step 0: Check existing settings

```bash
GLOBAL_PATH="$HOME/.fno/settings.yaml"; LOCAL_PATH=".fno/settings.yaml"
[[ -f "$GLOBAL_PATH" ]] && echo "global exists" || echo "no global"
[[ -f "$LOCAL_PATH" ]]  && echo "local exists"  || echo "no local"
```

If the target file already exists, AskUserQuestion: "Found existing settings.
Update in place, or start fresh?" Updating is non-destructive: `fno config set`
preserves every key it does not touch (and any unknown/extra keys), so you only
overwrite the answers the user changes.

## Step 1: Get the question plan

```bash
fno setup plan              # /setup and /setup local
fno setup plan --advanced   # /setup advanced
```

Parse the JSON. For each field, ask the user using its `question` text. Use the
`default` as the pre-filled answer and `default_source` as an inference hint:

- `repo-slug`  -> default from `basename $(git rev-parse --show-toplevel)`.
- `readme`     -> infer a one-line vision from the README's first paragraph.
- `auto-detect`-> detect from the repo (vault name, workspace topology).

The `always` set today is roughly: Obsidian on/off (+ vault name), project
vision, backlog id_prefix, external reviewer(s), auto-merge on/off. Ask only
what `fno setup plan` returns; do not invent extra questions.

## Step 2: Write each answer through `fno config set`

Write the GLOBAL scope by default; pass `--local` for `/setup local`:

```bash
fno config set <path> <value>            # global
fno config set <path> <value> --local    # project-scoped (/setup local)
```

`fno config set` coerces and schema-validates the value, then writes atomically
under a lock. Type handling:

- **bool**: `true` / `false`.
- **int**: a bare number.
- **list** (e.g. `config.review.external_reviewers`): a comma-separated string
  (`gemini,codex`) or a JSON array (`["gemini","codex"]`). An empty value is an
  empty list (external review disabled).
- **string**: as typed.

A rejected value (e.g. a reserved `id_prefix` like `tgt-`, or a length/charset
violation) exits non-zero and leaves the file unchanged. Re-prompt and retry;
NEVER hand-write the file to bypass validation.

Example writes:

```bash
fno config set config.obsidian.enabled true
fno config set config.obsidian.vault myvault
fno config set config.project.vision "A footnote-style delivery pipeline."
fno config set config.backlog.id_prefix myproj      # normalized; reserved families rejected
fno config set config.review.external_reviewers gemini,codex
fno config set config.auto_merge.enabled false
```

## Step 3: Workspace / project topology (`config.work.workspaces`)

The `config.work` map (workspace -> projects[]) is topology, not a scalar leaf,
so it is not asked via `fno setup plan`. When setting up a workspace,
auto-detect the current project and confirm it:

```bash
NAME=$(basename "$(git rev-parse --show-toplevel 2>/dev/null || pwd)")
# detect type/stack/package_manager from package.json / pyproject.toml / Cargo.toml / go.mod
```

Show a one-shot summary (name, path, type, stack, package manager) and confirm.
Offer to add related projects in the same workspace (loop, auto-detecting each).
This block is written as a single `config.work.workspaces.<slug>` map; if `fno
config set` cannot express the nested list in one call, write the `config.work`
block directly to the target settings file (it is the one structural
exception), then re-run `fno config doctor` to confirm it validates. The `name`
field under each project is the cross-project routing identity (what `fno mail
--to-project <name>` uses); it is distinct from `config.project.id`.

## Step 4: Post-merge parking-lot (per repo)

The post-merge ritual needs a repo-relative parking-lot path that is NOT derived
from the project name (the vault area often differs). Delegate to the dedicated
scaffold rather than guessing:

```bash
fno setup post-merge
```

## Step 4b: Offer global shell integration (OPTIONAL, consent-gated)

The mux (`fno mux`) auto-injects OSC 133 command-block markers into the shells
IT spawns (`config.mux.shell_integration: mux-panes`, on by default), so blocks
"just work" in mux panes with zero config and WITHOUT touching the user's rc.

Blocks in the user's OTHER terminals (iTerm, Terminal.app, a non-mux tab) need
the markers too. Offer to add ONE eval line to their shell rc - never silently,
always reversible:

```bash
# Detect their shell, then OFFER (ask [y/N], default no):
#   "Add OSC 133 block markers to your global <zsh|bash> rc so blocks work in
#    every terminal, not just mux panes? This appends one commented line to
#    ~/.zshrc (or ~/.bashrc). [y/N]"
# On y ONLY, append (idempotent - skip if _FNO_OSC133 already present):
grep -q _FNO_OSC133 ~/.zshrc 2>/dev/null || {
  printf '\n# fno OSC 133 block markers (remove this line + the next to undo)\n' >> ~/.zshrc
  echo 'eval "$(fno mux shell-init zsh)"' >> ~/.zshrc
}
```

Never edit the global rc without an explicit yes. `off` in `config.mux.shell_integration`
disables the mux-pane injection too; the manual `fno mux shell-init` eval always works.

Show the resulting config and validate:

```bash
fno config doctor
fno config get config.review.external_reviewers
```

For the full, generated reference of every key (type, default, doc), point the
user at `docs/configuration-guide.md` (regenerated by
`fno config schema --markdown`).

## What this wizard does NOT write

The following legacy keys are gone: `external_reviewer` (singular -
use the `external_reviewers` list), `budget_cap`, `commit_style`, `profile`,
`default_size`, `docs.*`, `schema_sources.*`, `linear.*`, `expertise`,
`plans.full_path`. They are not in the schema, so the wizard cannot write them.
A CI guard (`cli/tests/test_config_schema_drift.py`) fails the build if the
wizard ever surfaces a key that is not a real model leaf.

## Companions (RTK)

If RTK is detected (e.g. `~/code/dotfiles/bin/rtk-claude-hook.sh`), note that it
is already wired and do NOT run `rtk init -g` (it would double-wire). This is
informational only; it is not a `settings.yaml` key.

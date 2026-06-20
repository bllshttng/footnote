# Getting started with footnote

From install to your first shipped PR, then the handful of commands you'll actually run day to day.

## Install

In any Claude Code session:

```
/plugin marketplace add bllshttng/footnote
/plugin install fno@footnote
```

The postinstall hook puts the `fno` CLI on your PATH in a new session. Prefer the CLI standalone? `curl -fsSL fno.sh | sh`, `uv tool install fno`, `brew install bllshttng/fno/fno`, or `cargo install fno`. Full options: the [README](../README.md).

### Windows (WSL2)

footnote runs under [WSL2](https://learn.microsoft.com/windows/wsl/install), not native Windows. The loop leans on POSIX file locking, Unix sockets, and signals that Windows handles differently. WSL2 is real Linux, so everything here works unchanged inside it, and most Windows devs who'd want footnote already run their toolchain (and Claude Code) there.

One-time setup, from PowerShell as Administrator:

```powershell
wsl --install        # installs WSL2 + Ubuntu; reboot if prompted
```

Then open the Ubuntu shell and do everything from there: install `gh`, Python 3.11+, and `jq`, run Claude Code inside WSL2, and follow the install steps above. Keep your repos on the Linux filesystem (under `~/`), not `/mnt/c/...`; on the Windows mount, file locking and file watches are slow and unreliable.

### Verify it worked

```bash
fno --version          # prints a version
```

Inside Claude Code, type `/fno:` and you should see skill autocomplete (`target`, `think`, `blueprint`, ...).

```bash
claude /status         # shows your Claude account; if not, run: claude login
gh auth status         # authenticated; if not, run: gh auth login
```

## Configure your project

Configuration lives in `.fno/settings.yaml` (project-local) layered over `~/.fno/settings.yaml` (global). The global file holds shared defaults; the project file holds only the per-repo deltas. There are two ways to set it up.

**In a Claude Code session (agent-driven):**

```
/fno:setup
```

**In the terminal, no agent (CLI-native):**

```bash
fno setup wizard            # asks the few real per-project decisions, writes them validated
fno setup wizard --advanced # also surfaces the advanced settings
```

Both walk the same schema-derived question plan and write through the validated config writer, so a typo or an out-of-range value is rejected, not silently stored.

### Reading and editing config directly

```bash
fno config get config.review.required_bots          # read one value
fno config set config.auto_merge.enabled true       # set one key (atomic, schema-checked)
fno config set a.b=1 c.d=2                           # set several keys in one atomic call
fno config unset config.auto_merge.enabled          # remove a key (reverts to its default)
fno config doctor                                    # what resolved, and any suspicious values
```

`fno config set` also takes a whole block as JSON when you need it: `fno config set config.review '{"required_bots":["chatgpt-codex-connector"]}'`.

### The settings you'll touch first

These are real keys in `settings.yaml` (run `fno config get <key>` to read any of them, or `fno config schema --markdown` for the complete reference):

| Key | What it does | Default |
|-----|--------------|---------|
| `config.review.required_bots` | External review bots that must approve before `target` calls a PR done | none (no gate) |
| `config.review.external_reviewers` | Which reviewer(s) `pr check` waits on (e.g. `gemini`, `codex`) | none |
| `config.auto_merge.enabled` | Let `target` merge a PR itself once review passes | `false` |
| `config.target.defaults.max_iterations` | How many times `target` retries before stopping | `40` |
| `config.backlog.id_prefix` | The prefix for minted backlog node ids (e.g. `fno-a3f9`) | `ab-` |
| `config.obsidian.enabled` + `.vault` | Store plans and design docs in an Obsidian vault | off |
| `config.project.vision` | One line: what this codebase is and why (project-scoped) | none |

Budget and skip behavior are not config keys; they're flags you pass to a run, for example `/fno:target --budget 25 "..."` or `/fno:target --no-external "..."`. See [the target guide](guides/target.md) for the full flag list.

## Your first feature

### Option A: let target handle everything

```
/fno:target "add a health check endpoint that returns server status"
```

Target explores the design, plans it, implements with TDD, runs the internal review, and opens the PR. Watch it or walk away; it won't quit until the PR is open and CI is green. With no `config.review.required_bots` set there is no external-review gate; pin a bot there to make target also wait for a review pass.

### Option B: drive it step by step

```
/fno:think "health check endpoint"     # explore the design space, approve a direction
/fno:blueprint "health check endpoint"  # turn it into an executable plan
/fno:target path/to/plan.md             # execute the plan end to end
```

## The commands you'll actually run

These are the front door. Each is a skill (`/fno:<verb>` in Claude Code) or a CLI verb (`fno <verb>` in any terminal).

### Design and build

| Command | What it's for |
|---------|---------------|
| `/fno:think "X"` | Explore a design before building. Routes: default (design + acceptance criteria), `what-if` (stress-test failure modes), `panel` (multi-persona debate). |
| `/fno:blueprint "X"` | Turn an approved design into an executable plan with waves and tasks. |
| `/fno:target "X"` | The flagship loop: think to plan to code to review to a merge-ready PR. Point it at a feature, a plan path, or a backlog node id. |

### Review and ship the PR

The PR has a lifecycle, and there's a verb for each step. Full walkthrough: [the PR lifecycle guide](guides/pr-lifecycle.md).

| Command | What it's for |
|---------|---------------|
| `/fno:review` | Review a diff before you push. Default `sigma` is the internal six-agent panel; `peer` gets a cross-model second opinion (e.g. have Codex review Claude's code). |
| `/fno:pr create` | Open a PR from your commits (a Haiku worker writes the description). |
| `/fno:pr check` | Poll for the external review bot, then implement its feedback. |
| `/fno:pr merged` | The post-merge ritual: reconcile the backlog, run the retro, and file any follow-up work. Run it after a PR merges. |

`target` runs review, `pr create`, and (by default) `pr check` for you. Reach for the individual verbs when you're driving by hand or picking up a PR mid-flight.

### Work alongside other agents

footnote can spawn a worker on another provider and coordinate with it over a message bus. Quickstart: [the agents guide](guides/agents-quickstart.md).

```bash
fno agents spawn reviewer "review the diff on this branch" -p codex   # spawn a Codex peer
fno agents ask reviewer "what did you find?"                          # message it; it works on its own
```

For a one-off question to another model without keeping a peer around, spawn an ephemeral worker:

```bash
fno agents spawn q "summarize the failing tests" -p codex --once      # reply prints to stdout, then it tears down
```

Each agent runs its own loop; Claude, Codex, and Gemini, one project.

### Keep going past one feature

```
/fno:megawalk          # walk the backlog, shipping ready work until it's done or out of budget
```

`megawalk` reads a dependency graph (`fno backlog ...` manages it) and picks what ships next. Optional; `/fno:target "feature"` runs end to end with no backlog required.

## Next steps

- [Target pipeline](guides/target.md) - the full autonomous loop: flags, gates, cross-project, resume
- [Think and plan](guides/think-and-plan.md) - design exploration and planning
- [PR lifecycle](guides/pr-lifecycle.md) - review, create, check, merged
- [Agents quickstart](guides/agents-quickstart.md) - spawn and message peer agents
- [Troubleshooting](troubleshooting.md) - common failures and fixes
- [Best practices](best-practices.md) - reliable, cost-bounded runs
- [Security posture](security-posture.md) - what the pipeline will and won't do
- `CONTRIBUTING.md` - conventions if you plan to send a PR

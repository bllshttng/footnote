# Getting started with footnote

From install to your first shipped PR, then the handful of commands you'll actually run day to day.

## First five minutes

Install to a shipped PR in six steps.
The rest of this page fills in the details.

1. In a Claude Code session, install the plugin, then restart the session:
   ```
   /plugin marketplace add bllshttng/footnote
   /plugin install fno@footnote
   ```
2. Confirm the CLI is live:
   ```bash
   fno --version
   ```
   If that fails, install the Rust front door with `cargo install fno`.
3. Confirm GitHub is authenticated:
   ```bash
   gh auth status
   ```
   This is the most common cold-start failure.
   If it is not, run `gh auth login`.
4. Run the setup wizard and accept every default:
   ```bash
   fno config setup wizard
   ```
5. Point the loop at a small task:
   ```
   /fno:target "add a health check endpoint that returns server status"
   ```
6. Walk away.
   When it ships, the PR URL prints.

That is the whole path to a first PR.
When a step breaks, the sections below cover what to change.

## Install

In any Claude Code session:

```
/plugin marketplace add bllshttng/footnote
/plugin install fno@footnote
```

The postinstall hook puts the `fno` CLI on your PATH in a new session. Prefer the CLI standalone? `curl -fsSL fno.sh | sh`, `uv tool install fno`, or `brew install bllshttng/fno/fno` each install the published PyPI wheel, which bundles the complete set: the Rust `fno` front door, the three `fno-agents` binaries, and the Python CLI (`fno-py`). `cargo install fno` is the source route instead: it builds the Rust front door with your Rust toolchain, and the front door bootstraps the Python CLI on first use. Full options: the [README](../README.md).

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

If `fno` is "command not found", your install predates the complete payload or is a source/editable dev build (those are development artifacts and carry no binaries). Every supported channel - wheel, plugin postinstall, `fno.sh`, Homebrew - ships the Rust **`fno` front door**, which owns the mux and bootstraps the Python CLI. Repair: upgrade to a current release through your channel, or install the front door directly with `cargo install fno` (needs a **Rust toolchain**, `rustup`). When the front door is missing, a Claude Code session also reminds you. Until it is installed, reach the CLI directly as `fno-py`.

Inside Claude Code, type `/fno:` and you should see skill autocomplete (`target`, `think`, `blueprint`, ...).

```bash
claude /status         # shows your Claude account; if not, run: claude login
gh auth status         # authenticated; if not, run: gh auth login
```

## Configure your project

Configuration lives in `.fno/config.toml` (project-local) layered over `~/.fno/config.toml` (global). The global file holds shared defaults; the project file holds only the per-repo deltas. There are two ways to set it up.

**In a Claude Code session (agent-driven):**

```
/fno:setup
```

**In the terminal, no agent (CLI-native):**

```bash
fno config setup wizard            # asks the few real per-project decisions, writes them validated
fno config setup wizard --advanced # also surfaces the advanced settings
```

Both walk the same schema-derived question plan and write through the validated config writer, so a typo or an out-of-range value is rejected, not silently stored.

The terminal wizard also offers, defaulting to No, to wire footnote's SessionStart context into Codex and Gemini user config. You can do the same later with `fno config setup cli-hooks`.

### Reading and editing config directly

```bash
fno config get config.review.github_apps            # read one value
fno config set config.auto_merge.enabled true       # set one key (atomic, schema-checked)
fno config set a.b=1 c.d=2                           # set several keys in one atomic call
fno config unset config.auto_merge.enabled          # remove a key (reverts to its default)
fno config doctor                                    # what resolved, and any suspicious values
```

`fno config set` also takes a whole block as JSON when you need it: `fno config set config.review '{"github_apps":["chatgpt-codex-connector"]}'`.

### The settings you'll touch first

These are real keys in `config.toml` (run `fno config get <key>` to read any of them, or `fno config schema --markdown` for the complete reference):

| Key | What it does | Default |
|-----|--------------|---------|
| `config.review.github_apps` | External review bots that must approve before `target` calls a PR done; none set means no external gate | none |
| `config.review.posture` | How much review a code PR needs before it can merge; unset floors at `self_review` (nine-rung ladder) | none |
| `config.review.max_rounds` | Review rounds per PR, counted across its whole life | `2` |
| `config.review.external_reviewers` | Which reviewer(s) `pr check` waits on (e.g. `gemini`, `codex`) | `[]` |
| `config.auto_merge.enabled` | Let `target` merge a PR itself once review passes | `false` |
| `config.target.defaults.max_iterations` | How many times `target` retries before stopping | `40` |
| `config.backlog.id_prefix` | The prefix for minted backlog node ids (e.g. `fno-a3f9`); unset falls back to `ab-` | none |
| `config.obsidian.enabled` + `.vault` | Store plans and design docs in an Obsidian vault | `false` |
| `config.project.vision` | One line: what this codebase is and why (project-scoped) | none |

Budget and skip behavior are not config keys; they're flags you pass to a run, for example `/fno:target --budget 25 "..."` or `/fno:target --no-external "..."`. See [the target guide](guides/target.md) for the full flag list.

## Your first feature

### Option A: let target handle everything

```
/fno:target "add a health check endpoint that returns server status"
```

Target explores the design, plans it, implements with TDD, runs the internal review, and opens the PR. Watch it or walk away; it won't quit until the PR is open and CI is green. The internal review is the configured inline lane: one head-pinned reviewer by default (`config.review.posture` floors at `self_review`), with at most `config.review.max_rounds` rounds (default 2). With no `config.review.github_apps` set there is no external-bot gate; name a bot there to make target also wait for that review.

A green, reviewed PR is the finish line, not a merge. Target merges on its own only when you set `config.auto_merge.enabled` (default `false`); otherwise it stops and the merge is yours.

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
| `/fno:review` | Review a diff before you push. Default is the owned lane: one inline reviewer that emits a head-pinned attestation; `peer` gets a cross-model second opinion (e.g. have Codex review Claude's code). |
| `/fno:pr create` | Open a PR from your commits (a routed pr-create worker writes the description). |
| `/fno:pr check` | Poll for the external review bot, then implement its feedback. |
| `/fno:pr merged` | The post-merge ritual: reconcile the backlog, run the retro, and file any follow-up work. Run it after a PR merges. |

`target` runs review, `pr create`, and (by default) `pr check` for you. Reach for the individual verbs when you're driving by hand or picking up a PR mid-flight.

### Work alongside other agents

footnote can spawn a worker on another provider and coordinate with it over a message bus. Quickstart: [the agents guide](guides/agents-quickstart.md).

```bash
fno agents spawn "review the diff on this branch" --name reviewer -H codex   # spawn a Codex peer
fno agents ask reviewer "what did you find?"                          # message it; it works on its own
```

For a one-off question to another model without keeping a peer around, spawn an ephemeral worker:

```bash
fno agents spawn "summarize the failing tests" --name q -H codex --once      # reply prints to stdout, then it tears down
```

Each agent runs its own loop; Claude, Codex, and Gemini, one project.

### Keep going past one feature

The backlog is the queue. Capture work, see what's ready, ship it:

```bash
fno backlog idea "add webhook retries"   # capture work as a backlog node
fno backlog next                          # what's ready to ship now
/fno:target <node-id>                     # ship a ready node end to end
```

To work a whole board instead of one node: `/fno:target bg --all-ready` dispatches every ready, non-deferred node as background workers, and `fno backlog advance` (opt-in, merge-triggered) dispatches a node's dependents once its PR merges. The backlog is optional; `/fno:target "feature"` runs end to end with no backlog required.

## Keeping fno up to date

From a clone, the full refresh has three steps. `fno doctor update` refreshes the binaries. The long-running mux server and agents daemon keep old code until restarted:

```bash
git pull
fno doctor update          # reinstall the Python CLI + cargo binaries (fno mux + fno-agents) from source
fno agents restart --mux   # restart the daemon AND the mux server onto the fresh binaries
```

`fno agents restart` on its own restarts only the agents daemon (PTY workers survive). The `--mux` flag also restarts the mux server, which is **destructive** - it ends live mux sessions - so it is opt-in; reattach afterward. `fno doctor` flags a running mux server that predates the installed binary and reminds you to run it. In a running Claude Code session, bump the plugin (or relaunch) to pick up new skills/hooks after a pull.

- [Target pipeline](guides/target.md) - the full autonomous loop: flags, gates, cross-project, resume
- [Think and plan](guides/think-and-plan.md) - design exploration and planning
- [PR lifecycle](guides/pr-lifecycle.md) - review, create, check, merged
- [Agents quickstart](guides/agents-quickstart.md) - spawn and message peer agents
- [Troubleshooting](troubleshooting.md) - common failures and fixes
- [Best practices](best-practices.md) - reliable, cost-bounded runs
- [Security posture](security-posture.md) - what the pipeline will and won't do
- `CONTRIBUTING.md` - conventions if you plan to send a PR

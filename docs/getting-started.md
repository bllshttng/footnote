# Getting started with footnote

From install to your first shipped PR.

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

Inside Claude Code, type `/fno:` and you should see skill autocomplete (`target`, `think`, `blueprint`, ...). Then configure:

```
/fno:setup
```

### Verify your credentials

```bash
claude /status         # shows your Claude account; if not, run: claude login
gh auth status         # authenticated; if not, run: gh auth login
```

## Your first feature

### Option A: let target handle everything

```
/fno:target "add a health check endpoint that returns server status"
```

Target explores the design, plans it, implements with TDD, runs review, and opens the PR. Watch it or walk away - it won't quit until the PR is open and CI is green. By default there is no external-review gate; to make target also wait for a review bot, pin it under `config.review.required_bots`.

### Option B: step by step

```
/fno:think "health check endpoint"      # explore the design, approve it
/fno:blueprint "health check endpoint"   # plan it (hands off to a /target thread)
/fno:do path/to/plan.md                   # or execute the plan yourself
/fno:review                               # run the review panel
/fno:pr create                            # open the PR
```

## Configure for your project

`/fno:setup` writes `.fno/settings.yaml`. Key settings:

| Setting | What it does | Default |
|---------|-------------|---------|
| `config.max_iterations` | How many times target retries before stopping | 40 |
| `config.budget_cap` | Max spend in USD per target run | 25 |
| `config.review.required_bots` | Review bots that must approve before target is done | none (no gate) |
| `config.no_external` | Skip external review | false |
| `config.no_docs` | Skip doc generation | false |

`/target "feature"` runs end to end with no backlog and no vault required. The backlog (`/megawalk`) and Obsidian integration are optional; see the [README](../README.md).

## Next steps

- [Target pipeline](guides/target.md) - the full autonomous loop
- [Think and plan](guides/think-and-plan.md) - design before you build
- [Troubleshooting](troubleshooting.md) - common failures and fixes
- [Best practices](best-practices.md) - reliable, cost-bounded runs
- [Security posture](security-posture.md) - what the pipeline will and won't do
- `CONTRIBUTING.md` - conventions if you plan to send a PR

# Deployment Guide

Installation, setup, and multi-CLI deployment for the footnote plugin.

---

## Prerequisites

### Required

| Tool | Version | Purpose |
|------|---------|---------|
| Claude Code CLI | Latest | Primary runtime (`claude` command) |
| Git | 2.30+ | Version control, worktrees for cross-project |
| Bash | 4.0+ (macOS: via Homebrew) | Hook scripts, setup wizard |

### Optional (recommended)

| Tool | Version | Purpose |
|------|---------|---------|
| Python 3 | 3.9+ | Orchestrator CLI, agent sync scripts, roadmap tasks |
| jq | 1.6+ | JSON processing in hook scripts |
| yq | 4.x | YAML processing for settings and capabilities |
| gh | 2.x | GitHub CLI for PR creation (`/pr create` skill) |
| Node.js | 18+ | Context monitor hook (`context-monitor.js`) |

### Optional (per workflow)

| Tool | Purpose |
|------|---------|
| Linear CLI | Task management integration (`/linear` skill) |
| Gemini CLI | Multi-CLI support |
| Codex CLI | Multi-CLI support |

---

## Installation Methods

### Method 1: Plugin directory flag (development)

Best for active development on the plugin itself.

```bash
# Clone the repository
git clone https://github.com/<your-org>/fno.git
cd footnote

# Launch Claude Code with the plugin loaded
claude --plugin-dir /path/to/footnote
```

The `--plugin-dir` flag loads the plugin for that session only. You must pass it each time you invoke `claude`.

### Method 2: Symlink (permanent)

Best for daily use across all projects.

```bash
# Clone the repository
git clone https://github.com/<your-org>/fno.git

# Symlink into Claude Code's plugin directory
ln -s /path/to/footnote ~/.claude/plugins/footnote
```

Once symlinked, the plugin loads automatically in every Claude Code session.

### Method 3: Marketplace (coming soon)

The plugin manifest (`plugin.json` and `marketplace.json`) is structured for future marketplace distribution. When available:

```bash
claude plugin install footnote
```

---

## Setup Wizard

After installation, run the interactive setup wizard:

```
/fno:setup
```

There are two parts to setup:

**Shell script** (`scripts/setup.sh`) - runs automatically or manually:
1. **Preflight checks** - Verifies required tools are installed
2. **Global directory** - Creates `~/.fno/` for global config
3. **Checkpoints** - Creates `.fno/checkpoints/`
4. **Settings scaffold** - Creates `.fno/settings.yaml` with placeholder fields

**Interactive skill** (`/fno:setup`) - guides you through filling in the values:
- Project vision, goals, and constraints
- Pipeline config (max iterations, budget caps)
- External review provider (gemini, coderabbit, claude, codex)
- Notification preferences

### What setup creates

```
your-project/
  .fno/
    settings.yaml        # Project configuration
    checkpoints/         # State checkpoints
```

### Manual setup

You can skip the wizard and edit `.fno/settings.yaml` directly. See the [Configuration Guide](configuration-guide.md) for the full schema.

---

## Multi-CLI Setup

footnote supports three CLI providers. Skills are portable across all three; orchestration features (subagent dispatch, stop hook blocking) vary by provider.

### Claude Code (primary)

No additional setup needed beyond installation. Hooks are defined in `hooks/hooks.json` and load automatically.

Hook events used:
- **Stop** - target loop enforcement, conversation signal capture
- **PostToolUse** - Context monitoring
- **SessionStart** - Project vision injection

### Gemini CLI

1. Copy or symlink the extension into your Gemini extensions directory
2. The hooks file is `hooks/hooks-gemini.json`

Hook events used:
- **BeforeAction** - Initialize target state (runs once)
- **AfterAgent** - Do-target stop hook, conversation signal capture
- **SessionStart** - Project vision injection

Key differences from Claude Code:
- No native stop blocking (`supports_stop_blocking: false`)
- No subagent dispatch - runs sequential on main thread
- Supports experimental project-agent upgrade when `.gemini/agents/` directory exists
- Requires soft hook fallback for loop enforcement

### Codex CLI

1. Register the plugin in your Codex configuration
2. The hooks file is `hooks/hooks-codex.json`

Hook events used:
- **SessionStart** - Session bootstrap
- **PreToolUse** - Initialize target state (runs once)
- **Stop** - Do-target stop hook, conversation signal capture

Key differences from Claude Code:
- Uses custom agents instead of native subagents
- Agent sync script: `scripts/sync-codex-agents.py`
- `lifecycle_mode: hook_enhanced` (vs Claude's `hook_enforced`)

### Agent sync scripts

To keep agent definitions in sync across CLIs:

```bash
# Sync agents to Codex format
python scripts/sync-codex-agents.py

# Sync agents to Gemini format
python scripts/sync-gemini-agents.py
```

---

## CI/CD Integration

### Codex Bootstrap Check

The repository includes a GitHub Actions workflow at `.github/workflows/codex-bootstrap-check.yml`:

```yaml
name: Codex Bootstrap Check

on:
  pull_request:
  push:
    branches:
      - main

jobs:
  codex-bootstrap:
    runs-on: ubuntu-latest
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Setup prerequisites
        run: |
          sudo apt-get update
          sudo apt-get install -y jq

      - name: Run Codex setup (no package setup)
        run: |
          bash scripts/setup.sh --provider codex --skip-package-setup

      - name: Run doctor checks
        run: |
          bash scripts/doctor.sh
```

This workflow validates that the plugin bootstraps correctly on a clean Ubuntu environment with Codex as the provider.

### Adding your own CI checks

Useful scripts to run in CI:

```bash
# Validate plan format
bash scripts/validate-plan.sh path/to/plan/

# Validate TDD discipline
bash scripts/validate-test-first.sh

# Run doctor health checks
bash scripts/doctor.sh
```

---

## Plugin Directory Structure

The plugin expects this layout:

```
footnote/                          # Repository root
  .claude-plugin/
    plugin.json                     # Plugin manifest (name, version, description)
    marketplace.json                # Marketplace metadata
    CLAUDE.md                       # Plugin-level context
  plugins/
    footnote/                      # Core plugin directory
      skills/                       # 26 skill definitions (SKILL.md + references/)
      agents/                       # 12 agent definitions (.md files)
      commands/                     # Slash command wrappers
      hooks/                        # Hook scripts
        hooks.json                  # Claude Code hook definitions
        hooks-gemini.json           # Gemini CLI hooks
        hooks-codex.json            # Codex CLI hooks
        helpers/                    # Shared hook helper scripts
        target-stop-hook.sh       # Stop hook script
        session-start.sh            # Session bootstrap hook
        inject-project-vision.sh    # Vision injection
        context-monitor.js          # Context monitoring (Node.js)
      scripts/                      # Utility scripts
        setup.sh                    # Setup wizard
        run-target-loop.sh        # Cross-session loop runner
        validate-test-first.sh      # TDD validation
        validate-plan.sh            # Plan format validation
        sync-codex-agents.py        # Codex agent sync
        sync-gemini-agents.py       # Gemini agent sync
        discovery-brief.py          # Codebase analysis
        roadmap-tasks.py            # Task lifecycle management
        metrics/                    # Metrics analysis scripts
      CLAUDE.md                     # Plugin context for Claude Code
  .fno/                       # Runtime state (created by setup)
    settings.yaml                   # Project configuration
    checkpoints/                    # State checkpoints
  docs/                             # Documentation
  .github/workflows/                # CI/CD workflows
```

---

## Upgrading

### From git

```bash
cd /path/to/footnote
git pull origin main
```

If you installed via symlink, the update takes effect immediately in your next Claude Code session.

### Breaking changes

Check the [Changelog](changelog.md) for any breaking changes between versions. Key areas to watch:

- Hook event changes (new hooks added, existing hooks renamed)
- Settings schema additions (new required fields in `settings.yaml`)
- Agent definition changes (may require re-running sync scripts for Gemini/Codex)

### Re-running setup after upgrade

If the upgrade introduces new settings fields:

```
/fno:setup
```

The setup wizard is idempotent - it will not overwrite existing settings, only scaffold missing fields.

---

## Troubleshooting

### Health checks

Run the doctor script to verify your installation:

```bash
bash scripts/doctor.sh
```

Doctor checks include:
- Required tool availability (git, bash, jq)
- Plugin directory structure integrity
- Hook script permissions (executable bits)
- Settings file presence and validity
- Runtime configuration files present

### Common issues

**Plugin not loading**

```bash
# Verify the plugin directory structure
ls -la ~/.claude/.claude-plugin/plugin.json

# Or check the symlink target
ls -la ~/.claude/plugins/footnote
```

The plugin requires `plugin.json` to be present at `.claude-plugin/plugin.json` relative to the plugin root.

**Hooks not firing**

- Verify hook scripts have execute permission: `chmod +x hooks/*.sh`
- Check that `hooks.json` is valid JSON: `jq . hooks/hooks.json`
- For Gemini/Codex, verify the correct hooks file is referenced by your CLI configuration

**Stop hook not blocking exit**

The stop hook only blocks when:
1. `.fno/target-state.md` exists and shows `status: IN_PROGRESS`
2. The model's output does not contain a `<promise>` tag

If the hook is not blocking:
- Check that `.fno/target-state.md` exists
- Verify the status field is set to `IN_PROGRESS`
- Ensure the hook script is executable

**Setup fails with "command not found"**

```bash
# Install missing prerequisites
brew install jq yq gh  # macOS
sudo apt-get install jq  # Ubuntu/Debian
```

**Cross-session loop not re-entering**

The external loop runner (`scripts/run-target-loop.sh`) re-invokes the CLI until a `<promise>` tag appears or max iterations are reached. Verify:
- The script has execute permission
- The Claude Code CLI is in your PATH
- Max iterations has not been exceeded

---

## Environment Variables and Paths

### Variables used by hook scripts

| Variable | Set by | Purpose |
|----------|--------|---------|
| `CLAUDE_PLUGIN_ROOT` | Claude Code | Absolute path to the plugin root |
| `CODEX_PLUGIN_ROOT` | Codex CLI | Equivalent for Codex |
| `extensionPath` | Gemini CLI | Equivalent for Gemini |

### Key paths

| Path | Purpose |
|------|---------|
| `~/.claude/plugins/footnote/` | Permanent plugin installation |
| `~/.fno/` | Global config directory (signals, global settings) |
| `.fno/` | Project-scoped runtime state |
| `.fno/settings.yaml` | Project configuration |
| `.fno/target-state.md` | Active target pipeline state |
| `.fno/STATE.md` | Wave/task progress tracking |
| `.fno/SUMMARY.md` | Task completion notes |
| `.fno/00-INDEX.md` | Execution strategy for current plan |
| `.fno/checkpoints/` | State checkpoints |

---

## Provider-Specific Gotchas

### Claude Code

- **Stop hook is blocking** - the hook can prevent session exit. If you need to force-quit, the stop hook respects `<promise>` tags in output.
- **Subagent dispatch** - parallel wave tasks spawn as native subagents. High parallelism can hit API rate limits.
- **Context forking** - skills like `/pr create` run on cheaper models (Haiku) in isolated context. This is automatic and transparent.

### Gemini CLI

- **No stop blocking** - Gemini hooks cannot block session exit. The plugin falls back to soft enforcement (writing state to disk and relying on session-start to detect incomplete work).
- **No native subagents** - all execution is sequential on the main thread. Parallel waves are downgraded to sequential.
- **Dynamic agent upgrade** - if `config.gemini_experimental_agents` is enabled and `.gemini/agents/` exists, the runtime may elevate to project-agent mode.
- **Agent sync required** - after updating agent definitions, run `python scripts/sync-gemini-agents.py`.

### Codex CLI

- **Custom agents** - Codex uses its own agent format. Run `python scripts/sync-codex-agents.py` after agent changes.
- **Hook-enhanced lifecycle** - hooks enhance but do not fully enforce the pipeline. The orchestrator carries more responsibility for state management.
- **CI workflow** - the `codex-bootstrap-check.yml` workflow validates setup on every PR. Ensure your changes pass this check.

### All providers

- **Settings portability** - `.fno/settings.yaml` is provider-agnostic. The same settings file works across all three CLIs.
- **Script compatibility** - all shell scripts target `bash` and should work on macOS (with Homebrew bash) and Linux.

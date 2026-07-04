# footnote Plugin - Codebase Summary

## Repository Structure

```
footnote/                              # Flat root - plugin.json at .claude-plugin/
├── .claude-plugin/                     # Plugin manifest and metadata
│   ├── plugin.json                     # Plugin identity (name, version, author)
│   └── marketplace.json                # Marketplace listing metadata
│
├── skills/                             # 26 skills
│   ├── target/                          # Autonomous delivery pipeline
│   ├── plan/                           # Implementation plan generation
│   ├── do/                             # Lightweight single-session executor
│   ├── operator/                       # Wave-based task routing
│   ├── megawalk/                     # Multi-session task orchestration
│   ├── megaspec/                       # Iterative plan refinement
│   ├── think/                          # Design exploration, brainstorming
│   ├── what-if/                        # Scenario exploration, edge cases
│   ├── audit/                          # Multi-perspective feature audit
│   ├── tdd/                            # Test-driven development protocol
│   ├── fix/                            # Autonomous fix loop (one fix/iteration)
│   ├── debug/                          # Scientific method bug hunting
│   ├── speculate/                      # Parallel approach exploration
│   ├── sigma-review/                   # Multi-agent review orchestration
│   ├── create-pr/                      # PR creation from commits
│   ├── check-pr/                       # Poll for external review feedback
│   ├── distill/                        # Conversation analysis/distillation
│   ├── setup/                          # Interactive settings wizard
│   ├── codemap/                        # AST structural analysis (PageRank)
│   ├── ship-docs/                      # Architecture documentation generation
│   └── git-worktrees/                  # Isolated git worktree management
│
├── agents/                             # agent definitions
│   ├── archer.md                      # TDD task executor (Sonnet)
│   ├── code-reviewer.md                # Code quality reviewer (Opus)
│   ├── silent-failure-hunter.md        # Error swallowing detector
│   ├── type-design-analyzer.md         # Type invariant checker (Sonnet)
│   ├── integration-test-analyzer.md    # Journey test analyzer
│   ├── ux-flow-tester.md              # User journey simulator
│   ├── multi-device-checker.md         # Responsive/cross-device checker
│   ├── verifier.md                     # 3-level task verification (Haiku)
│   ├── goal-verifier.md                # Original goal validation
│   ├── tournament-debugger.md          # Competitive debugging
│   └── roadmap-generator.md            # Roadmap generation
│
├── hooks/                              # Lifecycle hooks
│   ├── hooks.json                      # Claude Code hook configuration
│   ├── hooks-gemini.json               # Gemini CLI hook configuration
│   ├── hooks-codex.json                # Codex CLI hook configuration
│   ├── target-stop-hook.sh              # Blocks exit during pipeline
│   ├── session-start.sh                # Session initialization
│   ├── context-monitor.js              # Context monitoring
│   └── helpers/                        # Hook helper scripts
│
├── commands/                           # 1 slash command
│   ├── cancel-target.md                 # Cancel active target pipeline
│
├── scripts/                            # Shell and Python automation
│   ├── lib/                            # Shared library scripts
│   └── metrics/                        # Cost tracking and analysis
│
│
├── docs/                               # Documentation
│   ├── architecture/                   # Architecture decision docs
│   ├── guides/                         # User-facing guides
│   └── providers/                      # Provider-specific docs
│
├── providers/                          # Provider abstraction layer
├── tests/                              # Test harnesses
│
├── CLAUDE.md                           # Root project instructions
├── AGENTS.md                           # Root agent definitions
├── GEMINI.md                           # Gemini-specific instructions
├── README.md                           # Public documentation
├── LICENSE                             # MIT License
└── .gitignore
```

## File Inventory

### By Directory

| Directory | Files | Primary Types | Purpose |
|-----------|-------|---------------|---------|
| `skills/` | ~80 | `.md`, `.sh`, `.py` | Skill definitions, references, scripts |
| `agents/` | 12 | `.md` | Agent persona definitions |
| `hooks/` | ~12 | `.sh`, `.json`, `.js` | Lifecycle hooks and platform configs |
| `commands/` | 6 | `.md` | Slash command definitions |
| `scripts/` | ~40 | `.sh`, `.py` | Automation, validation, testing |
| `.claude-plugin/` | 3 | `.json`, `.md` | Plugin manifest |
| `docs/` | ~5 | `.md` | Documentation |
| Root | ~8 | `.md`, `.json` | Config, README, license |

**Total files in plugin:** ~219

### By Type

| Type | Count | Percentage | Purpose |
|------|-------|------------|---------|
| Markdown (`.md`) | ~138 | ~63% | Skills, agents, commands, docs, references |
| Shell (`.sh`) | ~52 | ~24% | Hooks, scripts, validation, testing |
| Python (`.py`) | ~11 | ~5% | Orchestrator, agent sync, discovery |
| JSON (`.json`) | ~4 | ~2% | Hook configs, plugin manifest |
| YAML (`.yaml`) | ~3 | ~1% | Settings, plan frontmatter |
| Other | ~11 | ~5% | JavaScript, gitignore, license |

### Language Breakdown

```
Markdown    ████████████████████████████████  63%
Shell       ████████████████                  24%
Python      ███                                5%
JSON/YAML   ██                                 3%
Other       ██                                 5%
```

## Key Dependencies

### Required

| Dependency | Version | Purpose |
|------------|---------|---------|
| **Claude Code CLI** | Latest | Primary execution environment |
| **bash/zsh** | 4.0+ / 5.0+ | Hook scripts, automation |
| **git** | 2.30+ | Version control, worktrees, PR flow |
| **Python 3** | 3.9+ | Orchestrator (`operator/orchestrator.py`), agent sync |

### Optional (Recommended)

| Dependency | Purpose |
|------------|---------|
| **gh** (GitHub CLI) | PR creation, review polling, issue management |
| **yq** | YAML parsing in shell scripts (settings) |
| **jq** | JSON parsing in shell scripts (hooks, state) |
| **Gemini CLI** | Multi-CLI support (alternative execution environment) |
| **Codex CLI** | Multi-CLI support (alternative execution environment) |

### No Build Tools Required

The plugin has no build step. All files are interpreted at runtime:
- Markdown files are read directly by Claude Code as skill/agent definitions
- Shell scripts execute via bash/zsh
- Python scripts execute via the system Python interpreter
- JSON/YAML files are parsed by the respective CLI tools

## Entry Points

### Plugin Discovery

Claude Code discovers the plugin through one of two methods:

**Development mode (per-session):**
```bash
claude --plugin-dir /path/to/footnote
```

**Permanent installation (symlink):**
```bash
ln -s /path/to/footnote ~/.claude/plugins/footnote
```

### Plugin Manifest

`.claude-plugin/plugin.json` is the plugin identity file:
```json
{
  "name": "footnote",
  "version": "1.0.0",
  "description": "Autonomous development workflow: think - plan - do - review - ship.",
  "author": {"name": "Jason Noah Choi"},
  "keywords": ["workflow", "tdd", "planning", "code-review", "autonomous", "target"]
}
```

### CLAUDE.md Files

The repository uses multiple `CLAUDE.md` files at different levels, forming a context hierarchy:

| Path | Scope | Purpose |
|------|-------|---------|
| `/CLAUDE.md` | Root | Repository overview, architecture, key commands |
| `/CLAUDE.md` | Plugin | Plugin-level context for Claude |
| `/agents/CLAUDE.md` | Agents | Agent inventory context |
| `/hooks/CLAUDE.md` | Hooks | Hook configuration context |
| `/scripts/CLAUDE.md` | Scripts | Script inventory context |
| `/commands/CLAUDE.md` | Commands | Command wrapper context |
| `/skills/*/CLAUDE.md` | Per-skill | Skill-specific learned context |

### Slash Commands

Users interact with footnote through slash commands:

| Command | Skill | Purpose |
|---------|-------|---------|
| `/fno:target "feature"` | target | Full autonomous delivery pipeline |
| `/fno:blueprint "feature"` | blueprint | Create implementation plan |
| `/fno:do` | do | Lightweight single-session execution |
| `/fno:operator` | operator | Heavy wave orchestration |
| `/fno:think "feature"` | think | Design exploration |
| `/fno:sigma-review` | sigma-review | Multi-agent code review |
| `/fno:fix` | fix | Autonomous fix loop |
| `/fno:debug` | debug | Scientific method debugging |
| `/fno:tdd` | tdd | Test-driven development |
| `/fno:create-pr` | create-pr | PR creation from commits |
| `/fno:setup` | setup | Interactive settings wizard |

## State and Runtime Files

### `.fno/` Directory

The `.fno/` directory at the project root stores runtime state. It is partially gitignored (temporary state) and partially tracked (settings).

| File | Tracked | Purpose |
|------|---------|---------|
| `target-state.md` | No | Current pipeline iteration state |
| `STATE.md` | No | Wave/task progress tracking |
| `SUMMARY.md` | No | Task completion notes |
| `00-INDEX.md` | Varies | Execution strategy (from /blueprint) |
| `current-PLAN.md` | No | Active plan for target agents |
| `CONTEXT.md` | No | User constraints for current task |
| `settings.yaml` | Yes | Plugin settings |

### Internal Symlink

The `internal` symlink at the root points to an Obsidian vault:

```
internal -> /path/to/your/obsidian/vault/
```

This is used for plan storage and documentation in the author's development workflow. It is not required for plugin operation - users configure their own plan paths via `/fno:setup` or `.claude/settings.json`.

## Multi-CLI Configuration

The plugin supports three CLI environments with platform-specific hook configurations:

| CLI | Hook Config | Status |
|-----|-------------|--------|
| Claude Code | `hooks/hooks.json` | Primary, full support |
| Gemini CLI | `hooks/hooks-gemini.json` | Supported, sequential fallback |
| Codex CLI | `hooks/hooks-codex.json` | Supported, basic |

Skills (markdown-based) are portable across all three CLIs without modification. Hooks (shell scripts triggered by lifecycle events) require platform-specific configuration because each CLI has different lifecycle event names and invocation patterns.

Agent sync scripts (`scripts/sync-gemini-agents.py`, `scripts/sync-codex-agents.py`) translate agent definitions from Claude Code format to the target CLI format.

## Testing

The plugin includes test scripts but no formal test framework:

```bash
# Validate TDD compliance in commit history
./scripts/validate-test-first.sh

# Validate plan structure
./scripts/validate-plan.sh

# Scan for anti-patterns
./scripts/scan-antipatterns.sh

# Run orchestrator CLI
python skills/do/orchestrator.py --help

# Test hook behavior
./scripts/test_stop_hook_events.sh
./scripts/test-target-state-recovery.sh
./scripts/test-thrashing-detection.sh
```

## Quick Start

```bash
# Clone the repository
git clone https://github.com/<your-org>/fno.git

# Run with Claude Code (development mode)
claude --plugin-dir /path/to/footnote

# Or install permanently
ln -s /path/to/footnote ~/.claude/plugins/footnote

# Configure settings (optional but recommended)
# Then in Claude Code:
/fno:setup

# Build a feature
/fno:target "add user authentication with OAuth"
```

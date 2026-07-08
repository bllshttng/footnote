# Settings (.fno/config.toml)

**Load when:** configuring per-project defaults, looking up a config key's behavior, or onboarding a new project.

Unified config file with two sections: `work` (project topology) and `config` (execution defaults).

Lookup order (local wins):
1. `.fno/config.toml` — project-local override
2. `~/.fno/config.toml` — global defaults

## Example: project-local override

```yaml
# .fno/config.toml (project-local override)
config:
  expertise: frontend          # Default expertise injection
  max_iterations: 20           # Lower iteration cap for this project
  budget_cap: 25               # Max spend in USD (graceful stop when exceeded)

  # Phase toggles (all default to false = phase runs)
  no_external: false           # Skip external AI review
  no_docs: false               # Skip docs generation (advisory; never gates)
  no_browser: false            # Skip browser testing (advisory; never gates)
  no_how_to: false             # Skip how-to guide generation

  # Autonomous mode (unattended) defaults
  autonomous_max_turns: 15     # Max turns per session
  autonomous_budget: 25        # Budget per session in USD

  # Target / background-dispatch (Phase 2)
  target:
    auto_launch_on_blueprint: false  # opt-in, DEFAULT OFF. When true, finishing
                                     # /blueprint on a `ready`, non-deferred node
                                     # auto-dispatches it as a fresh `claude --bg`
                                     # /target worker (no-merge by default).
                                     # blocked/deferred/idea nodes are parked
                                     # (pre-planned future work, never launched).
                                     # Absent key reads as off (back-compatible).
                                     # Manual dispatch is always available via
                                     # `/target bg <node...>` regardless of this flag.
                                     # NOTE: this is a nested (dotted) key, so the
                                     # auto-launch gate reads it via `yq`. Without yq
                                     # installed it reads as off (fails closed/safe) -
                                     # the manual `/target bg` path has no yq dependency.

  # External review
  external_reviewer: gemini    # gemini | coderabbit | claude | codex | none

  notifications:
    enabled: true              # OS notifications on completion

  # Worktree isolation config
  worktree:
    env_files: [".env", ".env.local", ".env.development.local"]
    auto_install: true          # Set false to skip dep install (pnpm/uv/etc.)
                                # when target creates many worktrees of the same
                                # project — avoids materializing per-worktree
                                # .venv copies that bloat the uv cache (45GB+
                                # at scale). setup_command, when set, still
                                # runs regardless.
    setup_command: ""           # Custom setup (overrides auto-detect)
    test_command: ""            # Baseline verification after setup
    skip_verification: false
    speculate:
      max_variations: 5
      default_port_start: 3001
      dev_command: ""           # Overrides auto-detect for speculate
      cleanup_losers: true
```

## State Files Reference

| File | Purpose | Owner |
|------|---------|-------|
| `.fno/target-state.md` | Iteration tracking | target |
| `.fno/STATE.md` | Wave/task progress | /do waves |
| `.fno/SUMMARY.md` | Task completion notes | archer |
| `.fno/ledger.json` | Feature metrics | target |

## Cost Tracking

Source `scripts/metrics/cost-tracker.sh` for per-wave cost estimation during execution:

```bash
source "${CLAUDE_PLUGIN_ROOT}/scripts/metrics/cost-tracker.sh"
cost=$(estimate_cost "opus" "$input_tokens" "$output_tokens")
```

- Add `cost_estimate_usd` to each ledger.json entry
- Display running cost in stop hook system message

## Log Metrics

Append feature-level metrics to `.fno/ledger.json`: feature name, plan path, start/end time, token count, cost_estimate_usd, fork count, PR number, status, iterations.

The session-cost.py script ([pre-promise.md](pre-promise.md)) handles per-session cost calculation. Cost-tracker.sh is for finer-grained per-wave estimates inside a single session.

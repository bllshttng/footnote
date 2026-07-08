# Execution Modes

footnote has several ways to execute work. Pick the one that matches your situation.

**Size profiles** affect which executor target uses:
- `-S` (small) uses `/do` for lightweight single-session execution
- `-M` and `-L` use `/do waves` for wave orchestration with verification

Size profiles are independent of execution modes (main, agent, fork). You can combine them: `/target -L --agent "feature"` runs large ceremony with subagent dispatch.

## Quick Reference

| Mode | Command | Use when |
|------|---------|----------|
| **Target** | `/fno:target "feature"` | You want a full PR from an idea |
| **Target + plan** | `/fno:target path/to/plan/` | You already have a plan |
| **Operator** | `/fno:do waves` | Complex multi-wave plan with parallel tasks |
| **Do** | `/fno:do path/to/plan.md` | Quick focused task, no ceremony |
| **Cross-project** | `/fno:target --cross-project` | Feature spans multiple repos |

## Do - lightweight execution

```
/fno:do path/to/plan.md
```

Read the plan, make the changes, verify, done. No state machine, no quality gates, no PR creation. Just execute.

Best for:
- Bug fixes (2-3 files)
- Small focused features
- Tasks you'll review yourself

Do expects a single plan file (not a directory). If you point it at a directory, it suggests using operator instead.

## Operator - wave orchestration

```
/fno:do waves
```

Operator reads a plan directory with 00-INDEX.md and executes waves. It dispatches subagents for parallel tasks, tracks progress in STATE.md, and coordinates multi-phase work.

Best for:
- Plans with parallel tasks
- Features that touch multiple areas
- Work that benefits from subagent specialization (frontend agent, backend agent, etc.)

Operator is what target uses internally for the "do" phase. You can invoke it directly if you want orchestration without the full target ceremony (no review, no PR, no external review).

## Target - full pipeline

```
/fno:target "feature description"
```

Think, plan, do, review, validate, ship, external review, docs. The complete pipeline with quality gates. See the [target guide](target.md) for full details.

For unattended, walk-away execution, target's own loop combined with the `config.target.restart_after_n_turns` knob (Phase 6 daemon) handles fresh-context restarts so a long run doesn't degrade under compaction.

## Cross-project

```
/fno:target --cross-project "add auth to API and frontend"
```

Creates matching worktrees in each project defined in your workspace config, dispatches parallel subagents per project, and creates linked PRs. Backend projects execute first (order 1), frontend projects second (order 2).

Requires `~/.fno/config.toml` with workspace configuration. Run `/fno:setup` to set this up.

## How to choose

```
Simple bug fix?                  -> /fno:do
Quick feature, I'll review it?   -> /fno:do
Feature needs a plan?            -> /fno:blueprint, then /fno:do
Complex multi-phase feature?     -> /fno:target path/to/plan/
Starting from an idea?           -> /fno:target "the idea"
Touches multiple repos?          -> /fno:target --cross-project
Have a backlog of plans?         -> intake to the backlog, then /fno:megawalk
```

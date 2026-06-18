# Utilities: Debug, Code Review, and More

Beyond the pipeline, footnote includes tools for debugging and inspecting the autonomous loop state.

## Debug - systematic bug hunting

```
/fno:fix investigate "login fails on mobile Safari"
```

Debug uses the scientific method:

1. **Observe** - reproduce the bug, gather evidence
2. **Hypothesize** - form a falsifiable hypothesis
3. **Test** - run an experiment to prove/disprove
4. **Conclude** - log the finding, move to next hypothesis

If the first hypothesis fails, debug expands to multiple hypotheses. If those fail, it escalates to tournament mode (parallel agents testing different theories).

All attempts are logged to `.debug/attempts.jsonl` so future debug sessions skip already-disproven hypotheses.

### Chain debug into fix

```
/fno:fix investigate "the bug"
/fno:fix
```

Fix reads the debug findings and applies corrections iteratively. Each fix is committed before verification, so if it makes things worse, it reverts automatically.

## Code Review

```
/fno:review
```

Runs 6 specialized review agents in parallel on your changes:

| Agent | What it checks |
|-------|---------------|
| code-reviewer | Bugs, logic errors, project convention compliance |
| silent-failure-hunter | Swallowed errors, missing error handling |
| type-design-analyzer | Type invariants, encapsulation quality |
| integration-test-analyzer | Test coverage for integration points |
| ux-flow-tester | User journeys, error states, UI behavior |
| multi-device-checker | Responsive design, touch targets |

Each finding gets a confidence score. Only issues scoring above 80% are reported. This filters out false positives while catching real problems.

## Codemap (CLI verb)

```
fno codemap                          # Map current project, writes .fno/codemap.md
fno codemap --tokens 4000             # More detail (default: 2048)
fno codemap --db-schema               # Append DB schema (Supabase/Drizzle/Prisma)
fno codemap --orphans                 # List files with no inbound references
```

Runs AST analysis with PageRank to produce a weighted structural map of the most important symbols, module boundaries, and orphaned code. No LLM calls - just tree-sitter and math. The output file (`.fno/codemap.md`) is read by blueprint, target, operator, and sigma-review for structural context.

The `--db-schema` companion appends a `## Database Schema` section. It discovers a connection live-first: the shell `DATABASE_URL`, then a connection variable (`DATABASE_URL` / `POSTGRES_URL` / `SUPABASE_DB_URL` / `DIRECT_URL`) in a dev `.env` file (`.env.local`, `.env.development.local`, `.env.development`, `.env`), then localhost. `.env.production` / `.env.staging` are never auto-connected, all queries are read-only `pg_catalog` introspection, and the connection string is never echoed. A reachable database yields tables, columns, primary keys, enums, CHECK constraints, triggers, and foreign keys; with no reachable database it falls back to parsing migration files for tables and keys.

For higher-level discovery analysis (stack detection, conventions, patterns), see the `map-codebase` skill available in the `engineering` plugin.

## Tokens diagnostic (CLI verb)

```
fno tokens                           # diagnose current session token burn
fno tokens --json                     # JSON output for further analysis
fno tokens SESSION_ID                 # diagnose a specific session
```

Reports cache hit/miss patterns, idle gaps, resume-bug indicators, and cost attribution from your Claude Code transcript. Useful when a session feels expensive or when investigating cache breaks.

## Worktree lifecycle (CLI verb)

```
fno worktree status                  # list active worktrees with target status
fno worktree cleanup --older-than 7d  # remove stale worktrees
fno worktree archive <name>           # remove directory, keep branch
```

The actual worktree creation happens via Claude Code's native `EnterWorktree` / `ExitWorktree` tools (which fire the `WorktreeCreate` hook). This CLI exposes the bookkeeping subset.

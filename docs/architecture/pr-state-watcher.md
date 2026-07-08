# PR-state watcher (unified merge + review)

footnote automates the **merge** side of a PR's life from inside a session (`reconcile` -> `advance` on SessionStart), but the **review** side is manual and the only out-of-session merge automation was a per-repo launchd watcher that had to be installed once per repository. A `/target` session emits `MISSION COMPLETE` at PR-open + CI-green and terminates; once it dies, nothing watches the PR for a late review, and a GitHub web-button merge produces no local event at all.

The PR-state watcher is **one global launchd daemon** that watches every footnote PR for both terminal events - a new review and a web merge - and fires the right headless skill (`/fno:pr check` or `/fno:pr merged`) in that PR's repository. It replaces the install-once-per-repo model with a single watcher that follows the backlog graph, so a newly-created PR in any repo is covered with zero per-repo setup.

It is the canonical PR-state watcher and supersedes the per-repo post-merge watcher framework; the merge-fire responsibility of the per-repo watchers folds into it.

## Architecture

One LaunchAgent (`~/Library/LaunchAgents/sh.fno.pr-watcher.plist`) runs `fno pr-watch tick` on a `StartInterval` (default 600s). Global rather than per-repo because the PR record it iterates - the backlog graph at `~/.fno/graph.json` - is itself global and spans every repo.

The implementation is a Python package (`cli/src/fno/pr_watch/`) split along a pure/impure seam so the decision logic is exhaustively unit-testable:

| Module | Responsibility |
|---|---|
| `__init__.py` | `decide()` - the pure decision core over `(observation, watermark, reviewers, merge_ready)` |
| `_discover.py` | `discover_open_prs()` (open backlog nodes carrying a PR) + `read_pr_state()` (per-PR `gh` reads) |
| `_state.py` | `WatermarkStore` - the atomic per-PR watermark at `~/.fno/pr-watcher-state.json` |
| `_dispatch.py` | `fire_skill()` (headless `claude --print`) + `tick()` (the impure orchestrator) |
| `_install.py` + `cli.py` | the `fno pr-watch {tick,install,uninstall,status}` verbs + the gated plist installer |

### One poll cycle (a tick)

1. **Discover open PRs** from the global graph: backlog nodes with no `completed_at` carrying a `pr_number`. Each node's own `cwd` field resolves its local checkout; a PR whose repo is not checked out locally is skipped (a headless skill needs a working tree).
2. **Read PR state** per PR with `gh` (state + reviews/comments, handling the `[bot]` login suffix).
3. **Decide** (at most one action per PR per tick), in precedence order:
   - merged and not yet dispatched, and the post-merge readiness oracle passes -> fire `/fno:pr merged`;
   - closed-without-merge, or open past the max-age window -> park (poll it no further);
   - a configured reviewer posted activity newer than the watermark -> fire `/fno:pr check`;
   - otherwise no-op.
4. **Advance the watermark only after a clean dispatch.** A headless `claude --print` that exits 0 but reports `is_error: true` is a failure; the watermark is left unadvanced and the action is retried next tick, bounded to three retries before the PR is parked with a notification.

The daemon never merges, closes, comments on, or mutates a PR or a graph node - it only reads state and fires skills. Decisions emit canonical events (`pr_watch_dispatched`, `pr_watch_skipped`, `pr_watch_dispatch_failed`, `pr_watch_parked`) plus a per-tick heartbeat (`pr_watch_tick`) to the global event log under `~/.fno/`, so a quiet-but-alive watcher is distinguishable from a dead one.

### Headless fire

`claude --print --output-format json --dangerously-skip-permissions "/fno:pr <mode> <n>"`, run with `cwd` set to the PR's repo, default model Haiku. The plist carries `PATH` (captured at install time) + `HOME` in `EnvironmentVariables`; authentication rides the macOS keychain OAuth, so no API key is injected, and `--bare` is never used.

## Install is reviewed and gated

`fno pr-watch install` renders the plist, prints it in full, and requires explicit confirmation before writing anything to `~/Library/LaunchAgents/`. `--dry-run` (`-N`) prints the plist and writes nothing. The installer never runs `launchctl load`: a human reviews the rendered plist and loads it themselves. `fno pr-watch status` reports loaded/unloaded, last tick time, open-PR count, and parked PRs; `fno pr-watch uninstall` unloads and removes the plist while preserving the watermark store, so a reinstall does not re-fire history.

## Configuration

Under `config.pr_watch.*` in config.toml (all bounded, opt-in):

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | opt-in gate |
| `interval_seconds` | `600` | poll cadence (`> 0`) |
| `retries` | `3` | dispatch retries before parking (`>= 1`) |
| `max_age_days` | `14` | park a PR open longer than this (`>= 1`) |
| `model` | `claude-haiku-4-5` | model for the headless fire |

Review-dispatch only fires for PRs whose repo has configured reviewers; merge-dispatch is unconditional (subject to the readiness oracle).

## Coexistence and migration

The watcher is safe to run alongside the existing per-repo post-merge watchers and the SessionStart `reconcile`: a backlog node closed by `reconcile` is no longer open, so the tick's open-PR query no longer selects it, and where both a leftover per-repo watcher and the global watcher fire `/fno:pr merged`, the post-merge idempotency marker makes the second pass a no-op. Migration is therefore lazy - per-repo plists can be retired at any time without coordination; double-fire is harmless meanwhile.

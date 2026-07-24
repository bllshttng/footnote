# PR-state watcher (unified merge + review)

footnote automates the **merge** side of a PR's life from inside a session (`reconcile` -> `advance` on SessionStart), but the **review** side is manual and the only out-of-session merge automation was a per-repo launchd watcher that had to be installed once per repository. A `/target` session emits `MISSION COMPLETE` at PR-open + CI-green and terminates; once it dies, nothing watches the PR for a late review, and a GitHub web-button merge produces no local event at all.

The PR-state watcher is **one global launchd daemon** that watches every footnote PR for both terminal events - a new review and a web merge - and fires the right action in that PR's repository: the review poll fires the headless `/fno:pr check` skill, and a newly-observed merge runs the mechanical `fno pr ritual <pr> --autonomous` verb (directly as a subprocess, or warm-injected into the live origin session). It replaces the install-once-per-repo model with a single watcher that follows the backlog graph, so a newly-created PR in any repo is covered with zero per-repo setup.

It is the **sole** post-merge detector. `fno backlog reconcile` no longer dispatches a ritual: it closes merged nodes, stamps plans, and advances dependents, but the merge-to-ritual handoff is the watcher's alone.

## Architecture

One LaunchAgent (`~/Library/LaunchAgents/sh.fno.pr-watcher.plist`) runs `fno pr-watch tick` on a `StartInterval` (default 600s). Global rather than per-repo because the PR record it iterates - the backlog graph at `~/.fno/graph.json` - is itself global and spans every repo.

The implementation is a Python package (`cli/src/fno/pr_watch/`) split along a pure/impure seam so the decision logic is exhaustively unit-testable:

| Module | Responsibility |
|---|---|
| `__init__.py` | `decide()` - the pure decision core over `(observation, watermark, reviewers, merge_ready)` |
| `_discover.py` | `discover_open_prs()` (open backlog nodes carrying a PR) + `read_pr_state()` (per-PR `gh` reads) |
| `_state.py` | `WatermarkStore` - the atomic per-PR watermark at `~/.fno/pr-watcher-state.json` |
| `_dispatch.py` | `fire_skill()` (headless `claude --print` for the **review** poll only) + `tick()` (the impure orchestrator); merge dispatch delegates to `post_merge_route.dispatch_post_merge_ritual` |
| `_install.py` + `cli.py` | the `fno pr-watch {tick,install,uninstall,status}` verbs + the gated plist installer |

### One poll cycle (a tick)

1. **Discover open PRs** from the global graph: backlog nodes with no `completed_at` carrying a `pr_number`. Each node's own `cwd` field resolves its local checkout; a PR whose repo is not checked out locally is skipped (a headless skill needs a working tree).
2. **Read PR state** per PR with `gh` (state + reviews/comments, handling the `[bot]` login suffix).
3. **Decide** (at most one action per PR per tick), in precedence order:
   - merged and not yet dispatched, and the post-merge readiness oracle passes -> run `fno pr ritual <n> --autonomous` (warm-inject into the live origin if reachable, else the cold subprocess). The verb owns its own conditional headless judgment leg, so the watcher adds no model layer of its own and creates no background thread;
   - closed-without-merge, or open past the max-age window -> park (poll it no further);
   - a configured reviewer posted activity newer than the watermark -> fire `/fno:pr check`;
   - otherwise no-op.
4. **Advance the watermark only after a clean dispatch.** A headless `claude --print` that exits 0 but reports `is_error: true` is a failure; the watermark is left unadvanced and the action is retried next tick, bounded to three retries before the PR is parked with a notification.

The daemon never merges, closes, comments on, or mutates a PR or a graph node - it only reads state and fires skills. Decisions emit canonical events (`pr_watch_dispatched`, `pr_watch_skipped`, `pr_watch_dispatch_failed`, `pr_watch_parked`) plus a per-tick heartbeat (`pr_watch_tick`) to the global event log under `~/.fno/`, so a quiet-but-alive watcher is distinguishable from a dead one.

### Headless fire (review poll)

The review poll fires `claude --print --output-format json --dangerously-skip-permissions "/fno:pr check <n>"`, run with `cwd` set to the PR's repo, default model Haiku. The plist carries `PATH` (captured at install time) + `HOME` in `EnvironmentVariables`; authentication rides the macOS keychain OAuth, so no API key is injected, and `--bare` is never used.

A merge is not a headless claude fire. It runs the mechanical `fno pr ritual <n> --autonomous` verb as a bounded `fno-py` subprocess from the repo's canonical root (warm-injected as the identical command when the origin session is live). The verb owns the ritual's mechanical legs and its own conditional headless judgment one-shot, so the watcher never wraps the ritual in a `/fno:pr merged` LLM session or spawns a `--substrate bg` worker. Every dispatch attempt reserves a `post_merge_dispatch_receipt` (keyed by merge SHA) before acting - attribution and correlation only; route selection never reads it.

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

## Sole detector

The watcher is the only code that turns a newly-observed `MERGED` PR into ritual work. `fno backlog reconcile` and the SessionStart reconcile keep their node-closure job but no longer dispatch a ritual; the per-repo post-merge watcher framework is superseded. A merge the watcher hands off is deduped once by the per-merge-SHA marker plus the `post-merge-ritual:<sha>` TTL claim (and the ritual's own `reconcile:pr-<n>` claim guards re-entrancy), so a second observation of the same merge is a no-op. The marker layer is retained for a seven-day observation window after this cutover; only then may a trigger-based cleanup retire it, leaving the TTL claim as the single idempotency floor.

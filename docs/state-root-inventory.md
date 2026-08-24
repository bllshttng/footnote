# State-root inventory

Every writer that targets the top level of the state root (`config.state_dir`, default `~/.fno/`), with an owner and a lifetime.

A file nobody deletes is a file nobody owns. This page exists so a new root writer has somewhere to declare itself. It also lets the next person who finds an unexplained file look up whether anyone meant it.

Measured 2026-08-13 against one real install: 527 top-level entries, 395 of them context-nudge latches. The hook had landed six days earlier and deleted nothing. That is the failure this page is meant to prevent a second time.

## The rule

Anything that writes to the top level of the state root moves into a subfolder unless it genuinely belongs at the root. Anything unused gets removed.

"Belongs at the root" means one durable file per install, named for what it is: `graph.json`, `ledger.json`, `config.toml`. A family of files keyed by session, band, or timestamp does not belong there, however small each one is. The cost is legibility, not bytes. All 395 latches together were 14,625 bytes and made the directory unreadable.

Every location resolves through `fno.paths`. Adding a hardcoded `$HOME/.fno/<newdir>` repeats the bug one directory down, so route new paths through the resolver: `from fno import paths` in Python, `source "$(fno config paths shell-stub)"` in bash. `scripts/ci/check-no-hardcoded-paths.sh` gates this.

## Durable singletons

One file per install. These belong at the root.

| Entry | Writer | Lifetime |
|---|---|---|
| `graph.json`, `.lock`, `.sha256` | `graph/store.py` via `paths.graph_json()` | permanent |
| `graph.md` | `graph/_constants.py` | regenerated per write |
| `graph.html` | `graph/render_html.py` | regenerated |
| `graph-archive.json` | `graph/archive.py` via `paths.graph_archive_json()` | permanent |
| `relatedness.json` | `paths.relatedness_json()` | regenerated |
| `ledger.json` | `paths.ledger_json()` | permanent |
| `config.toml`, `.lock` | `paths.config_toml()` | permanent |
| `settings.yaml`, `.lock` | `fno/config/__init__.py` loader | permanent |
| `events.jsonl`, `.1` | `paths.global_events_json()`, rotated at 8 MB by `crates/fno-agents/src/events.rs` | rotated |
| `decisions.jsonl` | `paths.decisions_jsonl()`, written by `decide/__init__.py` | permanent |
| `questions.jsonl` | `paths.questions_jsonl()`, written by `fno inbox outstanding` | permanent; a question does not expire |
| `decisions.jsonl.corrupt` | `decide/__init__.py::_compact_index` | permanent; the only copy of a row whose source journal is gone |
| `decisions.jsonl.compact` | `decide/__init__.py::_compact_index` | transient; replaced onto `decisions.jsonl` in the same call |
| `evals-history.jsonl` | `paths.evals_history()` | append-only |
| `health-throttle.json`, `health-history.jsonl` | `health_monitor.py` | append-only |
| `convo-signals.jsonl` | `inbox/drain.py` | append-only |
| `recovery-nudges.json` | `recovery.py` | permanent |
| `watchdog-sweep.json` | `agents/watchdog.py` | permanent (rewritten per sweep) |
| `git-protection.json` | `hooks/git-protection.py` | permanent |
| `squads.json`, `.lock` | `crates/fno/src/squad_store.rs` | permanent |
| `session-names.json`, `.lock` | `agents/discover.py` | grows per session |
| `mux/` (`<session>.sock`, `.ver`, `.pid`, `.detach`, `.log`) | `crates/fno/src/proto.rs::mux_dir()`, following `config.state_dir` (`FNO_MUX_DIR` overrides) | server-managed; `kill-server` owns socket removal |
| `mux-view.json`, `.lock` | `crates/fno/src/view_store.rs` | permanent |
| `installed-rev`, `installed-rust-rev`, `source-path` | `update.py`, `doctor.py` | permanent |
| `my-priorities.md` | the operator, by hand or with their own `~/.fno/board.py` scratch script (not a repo file, and not `cli/src/fno/king/board.py`); read via `paths.operator_lane()` | permanent |
| `plugin-root` | `hooks/session-start.sh` | permanent |
| `pr-watcher-state.json` | `pr_watch/_state.py` | permanent |
| `pr-watcher-state-delivery.json` | `pr_watch/_dispatch.py` via `_delivery_state_path()` | permanent file, transient entries |
| `fleet-sweep-state.json` | `fleet_state.py`, written by the pr-watch tick's fleet leg | permanent file, transient entries |

`paths.locks_dir()` hardcodes `Path.home() / ".fno" / "locks"` on purpose, and a `config.state_dir` override deliberately does not move it. The config-free plan-stamp path and the config-loading append path have to agree on one directory, and moving it desyncs them. Its docstring says so. Do not "fix" it to match the rest of this page.

## Unrotated logs

Real writers, no rotation, no deleter. They stay at the root for now. Rotation is unclaimed work, and naming that here beats pretending they are fine.

| Entry | Writer | State |
|---|---|---|
| `ledger.md` | `cost/_register.py` | append-only, ~1 MB and growing |
| `pr-watcher.out.log`, `pr-watcher.err.log` | `pr_watch/_install.py` | unbounded |
| `groom.out.log`, `groom.err.log` | `backlog/groom.py` | unbounded |

## Session-scoped state

One file per session, per day, or per throttle window.

| Entry | Writer | Lifetime |
|---|---|---|
| `latches/.context-nudge-*` | `hooks/context-nudge.sh` | pruned by the same hook at `-mtime +2` |
| `.a2a-confirmed` | `agents/dispatch.py` | single file, overwritten |
| `.active-backlog-nudge` | `active_backlog.py`, `crates/fno-agents/src/active_backlog.rs` | single file |
| `.worktree-hook-root` | `hooks/session-start.sh` | single file |
| `.think-spawn-daily.json` | `provenance/spawn_think.py` | daily, overwritten |
| `.think-offer-cursor` | `hooks/born-with-why-offer-inject.sh` | single file |
| `.target-cancelled` | `hooks/helpers/init-target-state.sh`, `crates/fno-agents/src/loopcheck.rs` | consumed by the reader |
| `.preflight-cancel` | `scripts/ci/preflight.sh` | consumed by the reader (one-shot; stale after one hour) |
| `.reconcile-stamp`, `.reconcile-result.json`, `.shown` | `scripts/lib/reconcile-throttle.sh`, `hooks/reconcile-session-start.sh` | throttle window |
| `.plan-sync-watermark` | `plan/cli.py` | single file |
| `.path-migration-done` | `setup/migrate_paths.py` | one-shot sentinel |
| `.eval-sweep-stamp` | `scripts/lib/eval-sweep-throttle.sh` | throttle window |
| `.preflight-receipt-locks/` | `scripts/ci/preflight.sh` | live lock dirs |
| `mail-hold/<handle>.json` | `fno/mail/hold.py` via `paths.state_dir()` | one file per held session; deleted by the release timer, by `fno agents mail hold --off`, and by the turn-boundary tidy in `fno agents mail notify-self` |
| `locks/github-graphql-quota.lock` | `pr/_quota.py` via `paths.graphql_quota_lock()` | permanent empty sidecar; flock lives only for the probe-plus-command critical section |
| `bin/github-cli/gh`, `gh.pre-fno` | `setup/github_cli.py` via `paths.github_cli_proxy_dir()` | permanent proxy; one backup is retained only when an unrelated wrapper was present |

`.preflight-receipt-locks/` is the shape to copy. It derives from `dirname "$GLOBAL_EVENTS_PATH"`, so it already follows the resolver, and it sits at the root because the events file does.

`latches/` is the shape that had to be fixed. The hook that writes a latch also deletes it, because a latch keyed to a session id has no reader once that session ends. The lifetime lives with the writer, which is the only place it cannot go stale.

## Permanent repo-local directories

The rule above governs the state root, but its shape repeats inside a checkout: a directory nobody deletes is a directory nobody owns. A worktree that a script resets on every run is permanent by design. The sweep must keep it explicitly or churn its warm caches every pass. Recorded here so a sweep author checks this table before reaching for a blanket rule. A reader who finds the directory knows it is intentional.

| Entry | Writer | Lifetime |
|---|---|---|
| `.claude/worktrees/preflight` (or `<worktrees_base>/<repo>/preflight` when configured) | `scripts/ci/preflight.sh` | permanent; hard-reset to the candidate SHA each run, caches deliberately preserved |

The sweep's matching keep rule is `kept (permanent)` in `scripts/lib/worktree-lifecycle.sh`, keyed on the basename so it follows the worktree base wherever config puts it.

## Unattributed entries

Files present in one real install with no writer anywhere in the checkout. Recorded rather than deleted: an unexplained file is a finding, not a deletion, and a finding nobody wrote down gets rediscovered.

If you are cleaning up an install and hit one of these, find the writer first. If you confirm it is dead, delete the row here in the same change.

| Entry | Age when found | What is known |
|---|---|---|
| `.context-nudge-flush-rollout-<ts>-<uuid>` | written within four days | No source and no git history for the string anywhere in the repo. The writer is live and ahead of merged source. Check `fno doctor` for deployed-vs-source staleness before deciding. |
| `.ralph-cancelled` | - | No writer. `ralph` is a retired surface. |
| `do-ralph-stop-hook.log` | 5 months | Same retired surface. |
| `registry.json.lock` | 3 months, 0 bytes | The agents registry moved to `agents/registry.json`. |
| `keepalive.log` | 6 weeks | No writer found. |
| `fno-mode.sh` | 4 weeks | No writer found. A shell script living in the state root. |
| `zai-claude-settings.json` | 3 weeks | No writer. Referenced only by a test fixture. |
| `.env` | 3 weeks | No writer. Read it before deleting it and treat it as hostile: it can hold a credential. Never echo its contents into a log, a receipt, a fixture, or a PR body. |
| `evals.md` | 5 weeks | No writer found. |
| `SUMMARY.md` | 5 months | Flagged "investigate" by a 2026-04-22 audit and still present. |

## Foreign and cwd-relative debris

A `.fno/`, `.claude/`, `.abilities/`, or `.impeccable/` directory nested *inside* the state root is not a root writer. <!-- fno-rename-keep: historical pre-rename name, documented for forensic purposes --> Each holds project-relative paths written by a process whose working directory happened to be the state root. When `FNO_REPO_ROOT` is unset and `git rev-parse` fails, `paths.resolve_repo_root()` falls back to `Path.cwd()`. Foreign plugins do the same with their own literals.

Leave them. The finding is the cwd fallback, not the directories it produced. `.abilities` is the pre-rename state-root name, so anything under it predates the rename. <!-- fno-rename-keep: historical pre-rename name, documented for forensic purposes -->

## The project journal and `FNO_EVENTS_PATH`

The per-checkout journal `<repo>/.fno/events.jsonl` resolves through `paths.project_events_json()`. `FNO_EVENTS_PATH` overrides it. The override exists because repo-root resolution cannot be sandboxed. `fno.hermetic.neutralise` deliberately leaves `FNO_REPO_ROOT` unset. So an unpathed `append_event` under test writes a real row into the developer's checkout, and both operator readers fold that file. On 2026-08-17 six test fixtures sat in the needs panel beside two genuine operator questions.

`neutralise` pins the override at one line, and that env reaches the pytest, shell, and cargo trees. All three writers read it: the Python resolver, `scripts/lib/events.sh`, and `claim_events_path` in the Rust claims module. Rust has to read it because the two implementations share that journal and its `.lock.d` mutex as a wire contract. A pin one side ignores splits the writers apart. The loop-journal writers in `fno-agents` still build their path by hand and remain outside the pin. Reach for the pin, not for a marker the fold recognises. A fold that must know about test data carries an exception list, and the next fixture that misses the list refills the queue in silence. A test that sets `FNO_REPO_ROOT` itself and reads the journal back must name the same file in `FNO_EVENTS_PATH`. The pin outranks the root.

## Adding a new root writer

1. Prefer a subfolder. Reach for the root only for one durable file named after itself.
2. Add the accessor to `cli/src/fno/paths.py` so the location follows `config.state_dir`. When a bash caller needs it, export it from `cli/src/fno/setup/emit_shell.py`, then regenerate `scripts/lib/paths.sh`.
3. Name the deleter. Ephemeral state gets its lifetime in the code that writes it, not in a separate janitor. A janitor drifts from the writer and goes unrun. `scripts/prune-fno-dir.sh` was deleted for exactly that: never once invoked, while every file on its delete list sat in the root.
4. Add a row above.

## Project-relative session manifests

Not state-root writers, listed here because they are the other family of session-keyed files an operator finds and cannot attribute.

| Entry | Writer | Lifetime |
|---|---|---|
| `.fno/target-state.md` | `hooks/helpers/init-target-state.sh` via `fno do target init` | write-once per target session; archived on a terminal |
| `.fno/kings/<scope>.md` | `cli/src/fno/king/state.py` via coronation or `fno agents king init` | one loop-state file per live crown scope; stale files are inert without a live registry crown and cleanup is best-effort (`fno agents king done` on abdication) |

Target state is write-once after init. King state is atomically refreshed at coronation and both gate a stop hook.

A king runs in the canonical checkout. A target manifest can sit there too. So the king gets its own file rather than a `driver:` field on the target one. A manifest whose name says target and whose contents say king is how two sessions come to share one discriminator.

## Project-relative run journal

| Entry | Writer | Lifetime |
|---|---|---|
| `.fno/run-log.jsonl` | `crates/fno-agents/src/loopcheck.rs` through `run_state::append_transition` | append-only per checkout; retained as the lifecycle fold and deleted with a disposable worktree |

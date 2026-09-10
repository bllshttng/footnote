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
| `graph.db`, `graph.db-wal`, `graph.db-shm` | `crates/fno-agents/src/graph_sqlite.rs` | durable row store; WAL sidecars are SQLite-managed |
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
| `.canary` | `scripts/ci/check-state-canary.sh` `plant` | permanent, and written ONLY into a root with no live graph, which in practice means a CI runner. A live operator root is watched read-only and never receives it. One byte of content; its job is to give the walk a file it can prove it saw. |
| `notify-signals.json` | `crates/fno-agents/src/operator_notice.rs` (the notify_watch arm) | permanent; one entry per subscribed signal, the last token + ts; safe to delete (the next state change re-sends) |
| `watchdog-sweep.json` | `agents/watchdog.py` | permanent (rewritten per sweep) |
| `recovery/provider-outages.json`, `.lock`, `.provider-outages-*.tmp` | `agents/provider_outage.py` | permanent breaker/evidence journal; lock and atomic temp sidecars live only for one write and stale temps are safe to remove when no writer holds the lock; stores fingerprints, bounded raw refusal text, and explicit route IDs, never credentials or full transcripts |
| `recovery/provider-canaries/*.json` | `agents/watchdog.py` | bounded health proofs for audit; exact marker, provider/account IDs, pane ID, and timestamp only, never pane dumps or credentials |
| `recovery/canary-work/` | `agents/watchdog.py` | permanent empty neutral cwd reused by canaries; owns no node claim or project data |
| `recovery/transactions/outage-handoff-*.json` | `agents/outage_handoff.py` | permanent idempotency/phase journals per node and outage epoch; no credentials, transcript bodies, or pane dumps |
| `claims/dispatch%3A*.lock`, `.recovery.d/` | `claims/core.py` for provider handoff | transaction lease for one attempt; released at terminal return, recovery mutex removed by the claim primitive; contains holder/process metadata only |
| `<plan>.artifacts/target-state-*.md` | `state/outage_handoff.py` | permanent immutable handoff archive beside the plan; contains the pre-handoff session manifest but no credential or transcript payload |
| `git-protection.json` | `hooks/git-protection.py` | permanent |
| `squads.json`, `.lock`, `squads.json.tmp.*` | `crates/fno/src/squad_store.rs` (follows the mux state root; `FNO_AGENTS_HOME` overrides) | permanent; the pid-suffixed tmp is replaced on every locked write and a stale one is safe to remove |
| `session-names.json`, `.lock` | `agents/discover.py` | legacy alias overlay: registry rows' `aliases` are the primary name store and `reconcile` migrates file entries into rows; external roster rows (no fno row) keep the file as their alias home until the mail-address surface retires it |
| `agents/reap-receipts/<harness>-<session id>.json` | `crates/fno-agents/src/receipt.rs` (`write_reap_receipt`, called by the GC sweep in `gc_sweep.rs`; `FNO_AGENTS_HOME` overrides) | retained `config.agents.reap_receipts.retain_days` days (default 7); past the window the sweep strips the EXPENDABLE detail (ledger copy, per-effect rows, log path) and keeps the identity core (who, native locator, resume argv) forever, because deleting it would strand the only recovery record for a resumable session. Receipts carry a schema version, the writer build, and per-effect outcomes; `fno agents reap --verify --since 24h --json` audits them against the current build. A receipt whose `reaped_at` cannot be read is kept and named in the sweep summary, never deleted on a failed read |
| `mux/` (`<session>.sock`, `.ver`, `.pid`, `.detach`, `.log`) | `crates/fno/src/proto.rs::mux_dir()`, following `config.state_dir` (`FNO_MUX_DIR` overrides) | server-managed; `kill-server` owns socket removal |
| `mux/panes/<session>-<pane>.sock` | `crates/fno/src/pty.rs::keeper_dir()`, written by each `fno-agents-worker --pane` keeper | unlinked by the keeper when its child exits; a server-start sweep unlinks leftovers whose keeper is gone, and `fno mux pane keeper list` names them |
| `mux/threads/<agent>.sock` | `cli/src/fno/agents/dispatch.py::_lane_b_keeper_socket()`, written by each `fno-agents-worker --keeper` it launches (the pane-less lane-B thread keeper; a session-keyed subfolder, never a top-level write) | unlinked by the keeper when its child exits; no server-start sweep yet - the restart journey that owns re-adoption is a later group of the same epic, so until then a crashed keeper's leftover is named by the registry row's `messaging_socket_path` |
| `mux-view.json`, `.lock` | `crates/fno/src/view_store.rs` (follows the mux state root; `FNO_AGENTS_HOME` overrides) | permanent |
| `installed-rev`, `installed-rust-rev`, `source-path` | `update.py`, `doctor.py` | permanent |
| `my-priorities.md` | the operator, by hand or with their own `~/.fno/board.py` scratch script (not a repo file, and not `cli/src/fno/king/board.py`); read via `paths.operator_lane()` | permanent |
| `plugin-root` | `hooks/session-start.sh` | permanent |
| `pr-watcher-state.json`, `pr-watcher-state.lock` | `pr_watch/_state.py` | permanent |
| `pr-watcher-state-delivery.json` | `pr_watch/_dispatch.py` via `_delivery_state_path()` | permanent file, transient entries |
| `fleet-sweep-state.json`, `.lock` | `fleet_state.py`, written by the pr-watch tick's fleet leg | permanent file, transient entries |

`paths.locks_dir()` hardcodes `Path.home() / ".fno" / "locks"` on purpose, and a `config.state_dir` override deliberately does not move it. The config-free plan-stamp path and the config-loading append path have to agree on one directory, and moving it desyncs them. Its docstring says so. Do not "fix" it to match the rest of this page.

## Owned subfolders and remaining root state

Every subfolder and file below was found in the real root unnamed at the 2026-09-08 backfill and given a writer and a lifetime. When the root grows an entry this page does not name, `cli/tests/test_state_root_inventory.py` fails.

| Entry | Writer | Lifetime |
|---|---|---|
| `approvals.db` | `cli/src/fno/approvals/store.py` via `paths.state_dir()` | permanent SQLite store for approvals and effect attempts |
| `attest/` | `hooks/attest-model.sh`, `hooks/review-hold.sh` | one attestation sidecar per reviewed session |
| `backups/`, `graph.json.bak` | `crates/fno-agents/src/graph_store.rs::create_backup` (rotation, pruned to `GRAPH_BACKUP_KEEP`), the corrupt-read `.json.bak` copy, and `cli/src/fno/setup/migrate_paths.py` (`settings.yaml.bak.<ts>`) | graph rotation prunes itself; migration backups are one-shot per install. `graph.json.bak` is the pre-relocation sibling only builds older than this row write. |
| `briefs/` | `paths.briefs_dir()` | permanent sidecar discovery briefs |
| `bus/` | `paths.bus_dir()`, written by `cli/src/fno/bus/` (`messages.jsonl`, `cursors/`) | append-only mail log; each consumer's cursor is overwritten |
| `cache/` | `cli/src/fno/pr/_cache.py` (`cache/pr-status`) | regenerated PR-status cache |
| `events.jsonl.ephemeral` | `crates/fno-agents/src/claims.rs` (ephemeral retention class) | claim events whose retention class is ephemeral |
| `events.jsonl.shell-writers.d/` | `cli/src/fno/events/gc.py` | writer-liveness markers, GC'd with the journal |
| `failover-state.json`, `.lock` | `cli/src/fno/adapters/providers/failover.py`, `runtime_state.py` | permanent breaker state: storm-cap and no-swap-back phases |
| `graph.json.fts5` | `cli/src/fno/graph/fts.py` | derived full-text index beside the graph; regenerated, safe to delete |
| `graph.json.store.sock`, `graph-archive.json.store.sock` | `crates/fno-agents/src/graph_keeper.rs::store_socket_for` | server-managed IPC socket per store; unlinked by the keeper on exit and by the daemon's `store_socket_sweep` |
| `handoffs/` | `paths.handoffs_dir()` | handoff payloads; `scripts/handoffs-migrate-to-vault.sh` moves aged ones to the vault |
| `inbox/` | `paths.inbox_agents_root()` (`cli/src/fno/paths.py`), the mail bus's fallback root: one mailbox per agent handle under `agents/` | mail drains per handle; a drained envelope is acked away |
| `.interrupted-writes/` | `crates/fno-agents/src/daemon.rs` (quarantine) | writes caught mid-flight; released after the write settles |
| `lesson-candidates.jsonl` | `cli/src/fno/think_inspect.py`, `scripts/memory/append-lesson-candidate.sh` | append-only staging for the AGENTS.md pitfalls corpus; consumed by the monthly review |
| `logs/` | `cli/src/fno/agents/mux_spawn.py` | unrotated spawn logs |
| `mail-escalations/` | `cli/src/fno/mail/cli.py` (debounce markers via `O_CREAT|O_EXCL`) | one empty marker per sender/recipient pair inside the debounce window; safe to delete, the next escalation re-creates it |
| `MOVED-TO` | `cli/src/fno/paths.py`, `crates/fno-agents/src/paths.rs`, `state_path.rs` | migration pointer; permanent until an operator confirms the old path is gone |
| `notes/` | `cli/src/fno/research/core.py` (`notes/research`) | permanent research notes |
| `nudge-cursors/` | `cli/src/fno/agents/nudge.py` | one cursor per nudge target, overwritten |
| `observer-reports/` | the observer fold, via the `paths` accessor | one report per observation run |
| `operator-capture/` | `cli/src/fno/inbox/operator_turns.py` | raw operator turns awaiting `fno inbox operator ack` |
| `postmortems/` | the retro routine and stuck-terminal postmortem writer, via the `paths` accessor | permanent |
| `provider-runtime-state.json`, `.update.lock` | `cli/src/fno/adapters/providers/runtime_state.py` via the `paths` accessor | permanent; the update lock lives for one write |
| `providers/` | `cli/src/fno/adapters/providers/managed.py`, `staging.py` | permanent managed provider configs |
| `push-stamps/` | `hooks/git-protection.py` | one stamp per protected push |
| `relay-claude/` | Claude Code itself, via a `CLAUDE_CONFIG_DIR` account alias | operator-managed harness home. Never fno state, never swept. |
| `retro-pending/` | `paths.retro_pending_dir()`, written by `cli/src/fno/retro/sweep.py` | per-PR retro evidence awaiting harvest |
| `review-invocations/` | `cli/src/fno/review/invocation.py`, `crates/fno-agents/src/codex_inject.rs` | one invocation record per review round |
| `sessions/` | `scripts/save-session.py` | one transcript per saved session, written on demand |
| `spaces/` | the project-space layout (`cli/src/fno/paths.py`, `crates/fno-agents/src/state_path.rs`) | permanent; detailed in the project-space section below |
| `worktree-salvage/` | `hooks/worktree-salvage-ref.sh`, `scripts/setup/setup-worktree.sh` | salvage-mirror state per worktree |
| `worktrees/` | `fno agents workspace worktree ensure` under the worktree policy (`.claude/rules/worktrees.md`) | fno-managed external worktree base, `<repo>/<name>`; reaped on merge |
| `.worktree-stranded-cache.json`, `.worktree-stranded-refresh-stamp` | `hooks/worktree-peers-session-start.sh` | session-start cache, refreshed per throttle window |
| `board.py`, `board.sh`, `board_ids.py`, `board_render.py` | the operator, by hand (not repo files; the `my-priorities.md` row above already names `board.py`) | permanent operator tools |
| `validity-decks/` | `cli/src/fno/graph/maintain.py::write_validity_deck` (via `graph/cli.py`) | one deck per validity run, keyed by timestamp and node; permanent record |
| `.env` | the operator, by hand: the file's own header says it holds global fno secrets (model-routing keys) | until the operator rotates the key. Never echo its contents into a log, a receipt, a fixture, or a PR body, and read it only when the task requires it. |

## Machine and foreign junk

Not written by anything in this repo. Named so the gate can tell known junk from new drift.

| Entry | Writer | Lifetime |
|---|---|---|
| `.DS_Store`, `.metadata_never_index` | macOS Finder and Spotlight | regenerates on view; safe to delete |
| `.claude`, `.fno`, `.abilities`, `.impeccable` | foreign plugins and nested workspaces whose cwd was the state root | leave in place, per the foreign-debris section below | <!-- fno-rename-keep: historical pre-rename name, documented for forensic purposes -->

## Unclassified (follow-up filed)

Present in the real root, no writer found, not confirmable as dead inside this task's budget. The gate allowlists exactly these names. A follow-up node owns the verdict.

| Entry | What is known | Follow-up |
|---|---|---|
| `tailscale-migration/` | Operator's 2026-08-22 tailscale migration scripts and status snapshot. Infrastructure work, not an fno writer. | follow-up node "Rule on two operator-owned state-root leftovers", filed 2026-09-08 |
| `tools/` | One operator script, `wake.sh` (2026-08-15). Not written by the repo. | follow-up node "Rule on two operator-owned state-root leftovers", filed 2026-09-08 |

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
| `.target-cancelled` | `hooks/helpers/init-target-state.sh`, `crates/fno-agents/src/loop_target.rs`, `crates/fno-agents/src/loopcheck.rs` | consumed by the target reader |
| `.preflight-cancel` | `scripts/ci/preflight.sh` | consumed by the reader (one-shot; stale after one hour) |
| `.reconcile-stamp`, `.reconcile-result.json`, `.shown`, `.reconcile-result.json.tmp`, `.reconcile-result.json.shown` | `scripts/lib/reconcile-throttle.sh`, `hooks/reconcile-session-start.sh` (the hook's consume-after-show move creates the `.shown` rename; the throttle's `mv -f` creates the `.tmp` mid-write) | throttle window |
| `.plan-sync-watermark` | `plan/cli.py` | single file |
| `.path-migration-done` | `setup/migrate_paths.py` | one-shot sentinel |
| `.eval-sweep-stamp` | `scripts/lib/eval-sweep-throttle.sh` | throttle window |
| `.preflight-receipt-locks/` | `scripts/ci/preflight.sh` | live lock dirs |
| `mail-hold/<handle>.json` | `fno/mail/hold.py` via `paths.state_dir()` | one file per held session; deleted by the release timer, by `fno agents mail hold --off`, and by the turn-boundary tidy in `fno agents mail notify-self` |
| `route-settings/<sha16>.json` | `agents/model_routing.py::_write_settings_env_file` via `paths.state_dir()` (content-addressed, 0600, carries a live auth token) | one file per distinct route overlay, shared by every session on that route; a resume re-resolves provider-default tiers (`refresh_provider_default_tiers`), so a moved default yields a new file rather than a served stale one; files no registry row references and older than 14 days are pruned by `fno config route settings ls --prune` |
| `flight/<encoded key>.json` | `crates/fno-agents/src/single_flight.rs`, beside the claims dir it locks in (`$FNO_CLAIMS_ROOT`, else `$HOME`) | one file per distinct fno invocation; rewritten by each flight and read only inside the freshness window (`config.agents.single_flight_ttl_seconds`, default 10 s), so a leftover is inert rather than stale. Pruned past `ttl + join budget`, doubled, by `single_flight::prune_records` on the daemon's GC tick. Holds the child's stdout, which is the same text the verb prints on a terminal |
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

If you are cleaning up an install and hit one of these, find the writer first. If you confirm it is dead, delete the row here in the same change. The 2026-08-13 rows were confirmed dead and cleared by the 2026-09-08 backfill. `.env` graduated to a real row above, so no rows remain. The table returns here the next time an install produces one.

## Foreign and cwd-relative debris

A `.fno/`, `.claude/`, `.abilities/`, or `.impeccable/` directory nested *inside* the state root is not a root writer. <!-- fno-rename-keep: historical pre-rename name, documented for forensic purposes --> Each holds project-relative paths written by a process whose working directory happened to be the state root. When `FNO_REPO_ROOT` is unset and `git rev-parse` fails, `paths.resolve_repo_root()` falls back to `Path.cwd()`. Foreign plugins do the same with their own literals.

Leave them. The finding is the cwd fallback, not the directories it produced. `.abilities` is the pre-rename state-root name, so anything under it predates the rename. <!-- fno-rename-keep: historical pre-rename name, documented for forensic purposes -->

## The project journal and `FNO_EVENTS_PATH`

The per-repository journal `<space>/events.jsonl` resolves through `paths.project_events_json()`. `FNO_EVENTS_PATH` overrides it. The override exists because repo-root resolution cannot be sandboxed. `fno.hermetic.neutralise` deliberately leaves `FNO_REPO_ROOT` unset. So an unpathed `append_event` under test writes a real row into the developer's space, and both operator readers fold that file. On 2026-08-17 six test fixtures sat in the needs panel beside two genuine operator questions.

`neutralise` pins the override at one line, and that env reaches the pytest, shell, and cargo trees. All three writers read it: the Python resolver, `scripts/lib/events.sh`, and `claim_events_path` in the Rust claims module. Rust has to read it because the two implementations share that journal and its `.lock.d` mutex as a wire contract. A pin one side ignores splits the writers apart. The loop-journal writers in `fno-agents` still build their path by hand and remain outside the pin. Reach for the pin, not for a marker the fold recognises. A fold that must know about test data carries an exception list, and the next fixture that misses the list refills the queue in silence. A test that sets `FNO_REPO_ROOT` itself and reads the journal back must name the same file in `FNO_EVENTS_PATH`. The pin outranks the root.

## Adding a new root writer

1. Prefer a subfolder. Reach for the root only for one durable file named after itself.
2. Add the accessor to `cli/src/fno/paths.py` so the location follows `config.state_dir`. When a bash caller needs it, export it from `cli/src/fno/setup/emit_shell.py`, then regenerate `scripts/lib/paths.sh`.
3. Name the deleter. Ephemeral state gets its lifetime in the code that writes it, not in a separate janitor. A janitor drifts from the writer and goes unrun. `scripts/prune-fno-dir.sh` was deleted for exactly that: never once invoked, while every file on its delete list sat in the root.
4. Add a row above.

## The project space (`~/.fno/spaces/<slug>/`)

Project state left the checkout. One space per repository, keyed on the CANONICAL repo root (the git common dir's checkout), slug = the canonical path with `/` swapped for `-` (Claude's project-dir shape: read the directory name, see the path). Every worktree of a repo resolves to ONE space, so cross-worktree state needs no symlink. Per-worktree state sits at `<space>/worktrees/<worktree basename>/`. A checkout keeps only `.fno/config.toml` (committed project config) and the sandbox breadcrumb below. The first resolve of a moved file renames the legacy `<repo>/.fno/<file>` into the space and leaves a `<repo>/.fno/MOVED-TO` pointer naming it.

| Entry | Writer | Lifetime |
|---|---|---|
| `<space>/events.jsonl` | `paths.project_events_json()` and `fno-agents` journal writers | append-only per repository; rotated at 8 MB |
| `<space>/claims/` | `fno.claims` for repo-local keys (`walker:`, `review:`, `reap:`); global-id keys (`node:`, `dispatch:`, ...) stay at the global root | re-acquirable leases |
| `<space>/kings/<scope>.md` | `cli/src/fno/king/state.py` via coronation or `fno agents king init` | one loop-state file per live crown scope; stale files are inert without a live registry crown and cleanup is best-effort (`fno agents king done` on abdication) |
| `<space>/kings/<scope>.md.lock`, `.md.tmp` | `state.py` / `loop_king.rs` / `king/wake.py` over the manifest lock | lock lives only for the critical section; tmp is replaced on every locked write |
| `<space>/kings/<scope>.wake.json` | `pr_watch/_king_wake.py` (the tick's wake phase) | tick-local trigger cache with no reign meaning: `board_hash` + `board_rows` (the board-change trigger) and `answered_cursor` (the answered-escalation trigger); refreshed only when a wake fires, so it never outlives the manifest beside it |
| `<space>/kings/<scope>.wake.json.lock`, `.json.<pid>.tmp` | `pr_watch/_king_wake.py` over the manifest-lock helper | lock lives only for the sidecar's read-modify-write critical section; the pid-suffixed tmp is replaced on every locked write |
| `<space>/kings/<scope>.md.wake.log` | `pr_watch/_king_wake.py` (detached wake-mode walk) | append-only stdout of the walks this phase spawned; the events journal is the receipt, this log is diagnosis |
| `<space>/plans/` | `paths.plans_dir()` default (a configured vault template still wins) | permanent plan docs |
| `<space>/inbox/` | `paths.inbox_dir()` default | per-project inbox |
| `<space>/status-sinks/` | `paths.status_sinks_dir()` | per-sink cursors + error logs |
| `<space>/carveouts.jsonl`, `.lock.d/` | `cli/src/fno/carveout/core.py` via `paths.project_log` | append-only ledger; consumed by the retro-triage harvest |
| `<space>/worktree-log.jsonl` | `hooks/worktree-setup.sh` | append-only worktree lifecycle log |
| `<space>/wake-signals/` | `cli/src/fno/wake/signal.py` | one-shot records; drained by the wake readers |
| `<space>/artifacts/consolidated/` | `scripts/lib/consolidate-artifacts.sh` | per-PR consolidated gate artifacts, shared across worktrees |
| `<space>/scratchpad-adjacent diagnostics`: `loop-check.stderr.log`, `finalize.stderr.log`, `.loop-check-unavail-*`, `.king-resolve-unavail-*`, `.think-offer-cursor` | `hooks/target-stop-hook.sh`, `hooks/agy-target-stop-hook.sh`, `hooks/born-with-why-offer-inject.sh` | bounded retries/diagnostics; counters self-heal on the first clean decision |
| `<space>/worktrees/<name>/target-state.md` | `hooks/helpers/init-target-state.sh` via `fno do target init` | write-once per target session; archived on a terminal |
| `<space>/worktrees/<name>/run-log.jsonl` | `crates/fno-agents/src/loopcheck.rs` through `run_state::append_transition` | append-only per worktree; retained as the lifecycle fold and deleted with a disposable worktree |
| `<space>/worktrees/<name>/codemap.md` | `fno doctor codemap` | regenerated |
| `<space>/worktrees/<name>/scratchpad/` | `/target` sessions, per the manifest's `scratchpad_path` | live session scratch; archived at session end |

Target state is write-once after init. King state is atomically refreshed at coronation and both gate a stop hook.

A king runs in the canonical checkout. A target manifest can sit there too. So the king gets its own file rather than a `driver:` field on the target one. A manifest whose name says target and whose contents say king is how two sessions come to share one discriminator.

## The checkout-local exceptions

Two files stay inside a checkout, each because moving it breaks the thing that writes it:

| Entry | Writer | Lifetime |
|---|---|---|
| `.fno/config.toml` | `fno config project init` / the operator | committed project config, the same class as `.claude/settings.json` |
| `.fno/state-root-denied.json` | the DENIED worker, through `crates/fno-agents/src/claims.rs::write_state_root_breadcrumb` and its Python twin `cli/src/fno/claims/io.py::_breadcrumb_path` | until the next successful claim on this repo, which deletes it |
| `.fno/branch-provenance.json` | the pr-watch tick's stranded leg, via `cli/src/fno/branch_provenance_cache.py::write_cache` | overwritten every tick, never appended; safe to delete (the next tick rewrites it); the Kanban board's Branch Provenance section reads it |

The breadcrumb is here because it is the only thing a mute worker can still say.

A worker whose sandbox denies the fno state root has lost the claim store, the mail bus and the spawn mutex in one move. It cannot report that, because reporting is the capability it lost. It reports into its own transcript, which nothing reads, and the fleet meanwhile sees a live row with progress advancing. Five workers died that way in one night. Two of them finished real work nobody heard about.

The same sandbox that took the state root left the repo writable. So the refusal is written here, inside the one root the worker demonstrably has, and the operator reads it from outside the sandbox. Moving it into the space puts it behind the very grant that was denied. The write is best effort and never raises: a breadcrumb that cannot be written must not become a second failure stacked on the first.

It carries the denied absolute root, the harness session id, and a UTC timestamp. A successful claim write clears it, because a stale breadcrumb reads as a live problem forever.

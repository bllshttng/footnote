# State-root inventory

Every writer that targets the top level of the state root (`config.state_dir`, default `~/.fno/`), with an owner and a lifetime.

A file nobody deletes is a file nobody owns. This page exists so a new root writer has somewhere to declare itself. It also lets the next person who finds an unexplained file look up whether anyone meant it.

Measured 2026-08-13 against one real install: 527 top-level entries, 395 of them context-nudge latches. The hook had landed six days earlier and deleted nothing. That is the failure this page is meant to prevent a second time.

## The rule

The root holds folders, plus the rows this page already has. Nothing new lands at the top level: state goes in a named subfolder wherever one can hold it.

Two gates hold that line. At runtime, `hooks/claude-config-write-guard.sh` refuses an agent write creating a new top-level entry, in the state root or the Claude config dir, whatever its name. The refusal names the session's job tmp dir. In CI, `scripts/ci/check-state-root-rows.sh` fails any PR that adds a root pattern to this page against the checked-in baseline, so the root rows below are SHRINK-ONLY. A writer that moves into a subfolder or dies takes its row with it. A row never enters.

"Belongs at the root" used to mean one durable file per install, named for what it is: `graph.db`, `ledger.json`, `config.toml`. That set closed on 2026-09-27. A family of files keyed by session, band, or timestamp never belonged there, however small each one is. The cost is legibility, not bytes: all 395 context-nudge latches together were 14,625 bytes and made the directory unreadable.

Every location resolves through `fno.paths`. Adding a hardcoded `$HOME/.fno/<newdir>` repeats the bug one directory down, so route new paths through the resolver: `from fno import paths` in Python, `source "$(fno config paths shell-stub)"` in bash. `scripts/ci/check-no-hardcoded-paths.sh` gates this.

## Durable singletons

One file per install. These belong at the root.

| Entry | Writer | Lifetime |
|---|---|---|
| `ledger.json` | `paths.ledger_json()` | permanent |
| `config.toml`, `.lock` | `paths.config_toml()` | permanent |
| `settings.yaml`, `.lock` | `fno/config/__init__.py` loader | permanent |
| `events.jsonl`, `.1` | `paths.global_events_json()`; LEGACY bytes only. Since the event-store cutover every writer commits to `events.db` and no reader treats the file as authoritative; retained generations are imported on first store open | legacy import source |
| `decisions.jsonl` | `paths.decisions_jsonl()`, written by `decide/__init__.py` | permanent |
| `questions.jsonl` | `paths.questions_jsonl()`, written by `fno inbox outstanding` | permanent; a question does not expire |
| `decisions.jsonl.corrupt` | `decide/__init__.py::_compact_index` | permanent; the only copy of a row whose source journal is gone |
| `decisions.jsonl.compact` | `decide/__init__.py::_compact_index` | transient; replaced onto `decisions.jsonl` in the same call |
| `health-throttle.json` | `health_monitor.py` | permanent |
| `recovery-nudges.json` | `recovery.py` | permanent |
| `.canary` | `scripts/ci/check-state-canary.sh` `plant` | permanent, and written ONLY into a root with no live graph, which in practice means a CI runner. A live operator root is watched read-only and never receives it. One byte of content; its job is to give the walk a file it can prove it saw. |
| `opencode-install-<hash>.json` | `crates/fno-agents/src/opencode_install.rs` (the `plugin-install opencode` arm) | permanent; one per `OPENCODE_CONFIG_DIR` (the hash is of the config dir's canonical path), rewritten per install with one entry per written path and its content hash; removed last by `plugin-install opencode --uninstall`, so an interrupted uninstall is resumable |
| `watchdog-sweep.json` | `agents/watchdog.py` | permanent (rewritten per sweep) |
| `recovery/provider-outages.json`, `.lock`, `.provider-outages-*.tmp` | `agents/provider_outage.py` | permanent breaker/evidence journal; lock and atomic temp sidecars live only for one write and stale temps are safe to remove when no writer holds the lock; stores fingerprints, bounded raw refusal text, and explicit route IDs, never credentials or full transcripts |
| `recovery/provider-canaries/*.json` | `agents/watchdog.py` | bounded health proofs for audit; exact marker, provider/account IDs, pane ID, and timestamp only, never pane dumps or credentials |
| `recovery/canary-work/` | `agents/watchdog.py` | permanent empty neutral cwd reused by canaries; owns no node claim or project data |
| `claims/dispatch%3A*.lock`, `.recovery.d/` | `claims/core.py` for provider handoff | transaction lease for one attempt; released at terminal return, recovery mutex removed by the claim primitive; contains holder/process metadata only |
| `claims/<key>.lock.queue.d/`, `.priority.d/`, `.full.d/` | `crates/fno-agents/src/claim_queue.rs` owns the ticket format; waiters live in `crates/fno-agents/src/test_run.rs` and `scripts/ci/preflight.sh` | one ticket dir per live waiter on one admission door, named `NNNNNN/holder`; removed by its own waiter on every exit path and reaped by the next scan when its recorded pid is gone. The three dirs are the lanes: `priority` (a live `test:priority` claim names the checkout), `queue` (arrival order), `full` (whole-suite runs, suite door only) |
| `claims/build-waiters/` | `crates/fno-agents/src/test_run.rs` `CargoWait` | one marker per waiting checkout, removed when the wait ends and by the reader when its pid is gone; read by the stop hook to allow a stop during a held build |
| `agents/squads.json` | no writer in this build: a historical store location beside the live agent files (`registry.json` there IS authoritative, which is what makes the dead file read as real). `fno mux doctor` names it and its live replacement. | dead: nothing reads it, so it can only mislead; delete on sight (`fno mux doctor` prints the `rm`) |
| `agents/reap-receipts/<harness>-<session id>.json` | `crates/fno-agents/src/receipt.rs` (`write_reap_receipt`; the four writer doors are `gc_sweep.rs`, `roster_reap.rs`, the `update_registry` choke point in `receipt.rs`, and the Python choke point in `cli/src/fno/agents/registry.py`; `FNO_AGENTS_HOME` overrides) | retained `config.agents.reap_receipts.retain_days` days (default 7); past the window the sweep strips the EXPENDABLE detail (ledger copy, per-effect rows, log path) and keeps the identity core (who, native locator, resume argv) forever, because deleting it would strand the only recovery record for a resumable session. Receipts carry a schema version, the writer build, and per-effect outcomes; `fno agents reap --verify --since 24h --json` audits them against the current build. A receipt whose `reaped_at` cannot be read is kept and named in the sweep summary, never deleted on a failed read |
| `agents/fleet-stop.json` | `crates/fno-agents/src/fleet_incident.rs` via `AgentsHome::fleet_stop_json()` (written by `fno agents incident stop|clear`; `FNO_AGENTS_HOME` overrides) | permanent machine-wide circuit breaker: `{version, state: stopped|clear, generation, changed_at, changed_by, reason, holds, target, expires_at, origin, mail, mail_session_id}`, temp-plus-rename atomic. `incident stop|clear` and `loops pause-all|resume-all` use the same writer. `clear` is a positive record, never a deletion; every stop and clear increments `generation`. Read by the spawn gates, the active-backlog daemon, and the test-run owner BEFORE their bypass branches; a present-but-unreadable file refuses admission (`fleet-stop-unavailable`), and a writer refuses to replace an unreadable record - remove it by hand to start a fresh generation. Mail is not gated by the fleet breaker; pause-all records whether it armed a clock and the full owning session id, while the registry delivery_policy flag enforces a mail hold. A failed owner release stays `armed` for a later `resume-all` retry; success marks it `lifted` |
| `agents/machine-process-snapshot.txt`, `.active` | `crates/fno-agents/src/machine_sample.rs::maybe_capture_process_snapshot` via the daemon's `machine_watch` arm (`FNO_AGENTS_HOME` overrides) | newest successful full process snapshot, capped at 1 MiB and mode 0600, retained across reboots and overwritten on the next pressure episode; `.active` marks the current episode and is removed after a measured sample falls below both process thresholds |
| `agents/fleet-stop.d/<target>.json` | `crates/fno-agents/src/fleet_incident.rs` (same writer as `agents/fleet-stop.json`; `FNO_AGENTS_HOME` overrides) | one 0600 session or territory record per target under a 0700 directory; expiry releases enforcement but records remain visible in status until clear or re-arm; session identity uses the full session id |
| `agents/test-pause.json`, `.lock` | `crates/fno-agents/src/test_hold.rs::reconcile` via `AgentsHome::test_pause_json()` (run by `fno agents incident stop|clear` and the daemon's `machine_watch` tick; `FNO_AGENTS_HOME` overrides) | lives only while the machine-wide breaker holds `tests`: `{generation, paused: [[pid, birth]], announced}`, the fleet test processes the hold sent SIGSTOP. The lift sends SIGCONT to exactly those incarnations, announces the all-clear, and deletes the file; the lock sidecar persists |
| `agents/otel/` (`port`, `otel.db`) | `crates/fno-agents/src/otel_ingest.rs` via the daemon's otel arm (`FNO_AGENTS_HOME` overrides; `[telemetry] claude_otel = false` skips the arm, removing `port` on shutdown) | grows: one `api_requests` row per API request reported by a birthed supervisor, harness-neutral columns (session, model, token and cost micros, skill/plugin/agent names); no rotation in this PR. Nothing leaves the machine |
| `~/.fno/loops-paused.json` | read by `crates/fno-agents/src/loops_pause.rs`; no active writer | legacy pause-all sentinel, read-only during migration and deleted by `loops resume-all`; `pause-all` refuses to stack a bounded breaker pause over an active or corrupt sentinel |
| `agents/compacting/<session>.json` | `crates/fno-agents/src/compaction.rs` via `compaction::mark` (the `PreCompact` hook's best-effort call to `fno-agents compaction mark --session`; `FNO_AGENTS_HOME` overrides) | session-keyed, overwritten per compaction, never cleaned: a stamp is tiny and self-expiring (the reader returns `stamp-past-ceiling` after 45 minutes), so no sweep is owed. Read by `compaction::compaction_state` and the provider-cap actor, which holds any member whose state is `Compacting` or a live-stamp `Unknown` |
| `agents/provider-cap/<lane>-<epoch>.jsonl` | `crates/fno-agents/src/provider_cap.rs` (`FNO_AGENTS_HOME` overrides) | per-move journal: one line per migration step (`decided`, `destination`, `spawn-confirmed`, `stopped`, `unknown`, the wave-4 return steps `canary-resumed`, `canary-verdict`, `trickle-resumed`, `announced`, `return`), written append-only at actor time. A step the code could not prove records `unknown`, never `moved` (the unmeasured-state rule). The per-epoch canary state `return-<lane>.json` (keyed by the reset it belongs to) and the `decide` answers live beside it under the same folder |
| `mux/` (`<session>.sock`, `.ver`, `.pid`, `.detach`, `.log`, `mux-view.json`, `mux-view.json.lock`, `squads.json`, `squads.json.lock`, `session-names.json`, `session-names.json.lock`) | `crates/fno/src/proto.rs::mux_dir()` owns the sockets; the three sidecars move here at the server's own start (`crates/fno/src/state_layout.rs::migrate_mux_sidecars_at`, wired into `server.rs::serve` before the first store opens); `crates/fno/src/view_store.rs` and `squad_store.rs` resolve through `proto::mux_sidecar_path`, the agents crate reads `squads.json` and `session-names.json` through `place`, and `agents/discover.py` points at `mux/session-names.json`. `FNO_AGENTS_HOME` overrides both stores | server-managed; `kill-server` owns socket removal |
| `mux/command-receipts/<request-id>.json` | `crates/fno/src/mux_cli/harness_command.rs` via the mux directory | one idempotent native-action receipt per request; retained for seven days, then removed by the next command invocation |
| `mux/panes/<session>-<pane>.sock` | `crates/fno/src/pty.rs::keeper_dir()`, written by each `fno-agents-worker --pane` keeper | unlinked by the keeper when its child exits; a server-start sweep unlinks leftovers whose keeper is gone, and `fno mux pane keeper list` names them |
| `mux/themes/<name>.toml` | `crates/fno/src/theme_import.rs` (the Settings importer) or the user by hand | permanent, user-owned theme; delete the file to remove it |
| `mux/threads/<agent>.sock` | `cli/src/fno/agents/keeper_thread.py::_lane_b_keeper_socket()`, written by each `fno-agents-worker --keeper` it launches (the pane-less lane-B thread keeper; a session-keyed subfolder, never a top-level write) | unlinked by the keeper when its child exits; no server-start sweep yet - the restart journey that owns re-adoption is a later group of the same epic, so until then a crashed keeper's leftover is named by the registry row's `messaging_socket_path`. The state root follows the daemon's derivation, not just `state_dir()`: the Rust registry-side keeper sweep derives the threads dir from the agents root's parent (`FNO_AGENTS_HOME`'s parent when set, else `state_dir()`), so the spawn must write the socket where that sweep reads it or a restart rebind silently finds nothing. The override arm keeps `FNO_AGENTS_HOME`'s literal spelling - no `resolve()`: the sweep matches the row's socket path byte-for-byte against a dir built from the raw `--home` string, and resolving repoints it through symlinked components (macOS `/var` -> `/private/var`), leaving the socket orphaned at every restart |
| `my-priorities.md`, `.lock` | the operator, by hand or with their own `~/.fno/board.py` scratch script (not a repo file, and not `cli/src/fno/king/board.py`); read via `paths.operator_lane()` | permanent |
| `pr-watcher-state.json`, `pr-watcher-state.lock`, `.lock` | `pr_watch/_state.py` | permanent |
| `pr-watcher-state-delivery.json`, `.lock` | `pr_watch/_dispatch.py` via `_delivery_state_path()` | permanent file, transient entries |
| `fleet-sweep-state.json`, `.lock` | `fleet_state.py`, written by the pr-watch tick's fleet leg | permanent file, transient entries |

`graph.json` is retired. A former file moves to `backups/graph.json.retired.*`; an unimported nonempty file makes the store refuse to open so its rows remain recoverable.

`paths.locks_dir()` hardcodes `Path.home() / ".fno" / "locks"` on purpose, and a `config.state_dir` override deliberately does not move it. The config-free plan-stamp path and the config-loading append path have to agree on one directory, and moving it desyncs them. Its docstring says so. Do not "fix" it to match the rest of this page.

## Frozen root rows (2026-09-27 backfill)

The 2026-09-27 sweep found 71 undocumented top-level entries on one real root. The crown verified and deleted the backups whose data a `.db` provably holds. The ephemeral writers below learned to clean up after themselves. Every remaining real entry got its row here. The section is FROZEN: shrink-only, like every root row. When one of these writers moves into a subfolder, delete its row in the same PR.

| Entry | Writer | Lifetime |
|---|---|---|
| `config.toml.bak*` (5 files, 2026-09-08 through 2026-09-22) | the operator, by hand, during config and model-routing edits | permanent: no `.db` holds their data, so deletion waits on the operator's yes |
| `ruleset-21074865-before-smoke-hold.json` | the operator, by hand, before a smoke-hold change | permanent, same rule as the `config.toml.bak*` row |
| `pr-watch-bounce.json` | `cli/src/fno/pr_watch/_install.py` | transient bounce record, overwritten per deferred bounce |
| `ntfy/` | an operator-run ntfy server (its `cache.db`) plus `ntfy.out.log` / `ntfy.err.log` | server-managed; not written by this repo |
| `intel/` | the intel fold behind `fno intel` | regenerated per run; safe to delete |
| `sidecar/` | `cli/src/fno/paths.py::sidecar_dir()` | per-item sidecar files owned by their writers |
| `blueprinters/` | the blueprinter sessions (one hash dir per session) | session-keyed; a dead session's dir is inert |
| `reign-watch/` | the reign-watch tool deploy (its `bin/`, `src/`, and `fno-mux` copy) | operator-managed |
| `jobs/` | event-snapshot dirs from isolation and repro runs (`events-global-<date>/`) | repro residue; safe to delete once a run ends |
| `backup/` | the operator, by hand: pre-store graph exports (`fno.json`, `etl.json`, node-list jsons) | permanent until the operator rules on them |
| `stable-bin/` | the operator, by hand: a stable-channel binary copy (`bin/`, `src/`) | operator-managed |
| `internal/` | the operator, by hand: vault-adjacent exports (`etl.json`, `fno.json`) | operator-managed |
| `handoff-evidence/` | the rank goldens capture (`crates/fno-agents/src/backlog/rank_cli.rs` names the goldens) | permanent evidence for the rank CLI |
| `repro-isolated/` | `cli/src/fno/observer/isolation.py`: the observer's isolated repro root | repro residue; safe to delete once a run ends |
| `.codegraph/` | codegraph (foreign tool): its index beside the state root | tool-managed; never swept |
| `.gitignore` | the operator, by hand | permanent |

## Named root exceptions

The migration moves every movable file into a named subfolder (`docs/state-root-layout.tsv` is the table, and `fno-agents state migrate` is the mover). These stay at the top level, each because a move is blocked on a port, an operator ruling, or a platform:

| Entries | Why they cannot move yet |
|---|---|
| `config.toml`, `config.toml.lock`, `config.toml.bak*`, `settings.yaml`, `settings.yaml.lock` | The config contract every harness, doc and OSS install names at `~/.fno/config.toml`. The `.bak*` files await the operator's ruling on their deletion. |
| `my-priorities.md`, `my-priorities.md.lock`, `ruleset-21074865-before-smoke-hold.json` | Operator-owned, edited by hand at that path. |
| `.env`, `.gitignore` | Operator-owned, edited by hand at that path. |
| `.DS_Store`, `.metadata_never_index` | macOS Finder and Spotlight; Spotlight reads the opt-out marker only at the folder root. |
| `.path-migration-done` | The Python startup migration's sentinel (`cli/src/fno/cli.py`). Parking it re-fires that migration, which rewrites root files. It moves when that migration is deleted or ported. |
| `ledger.json`, `ledger.md`, `events.jsonl`, `events.jsonl.1`, `events.jsonl.ephemeral`, `events.jsonl.shell-writers.d/`, `decisions.jsonl`, `decisions.jsonl.compact`, `decisions.jsonl.corrupt`, `questions.jsonl` | Python locators. `paths.py` derives the journals from the ledger's parent, and about 18 Python sites hardcode `state_dir() / "events.jsonl"`. Over the 30-line budget; they move when those callers port. Their `.db` stores DO move (Rust derives them). |
| `pr-watcher-state.json`, `pr-watcher-state.json.lock`, `pr-watcher-state.lock`, `pr-watcher-state.lock.lock`, `pr-watcher-state-delivery.json`, `pr-watcher-state-delivery.json.lock`, `pr-watch-bounce.json`, `fleet-sweep-state.json`, `fleet-sweep-state.json.lock`, `provider-runtime-state.json`, `provider-runtime-state.json.update.lock`, `failover-state.json`, `failover-state.json.lock`, `health-throttle.json`, `recovery-nudges.json`, `watchdog-sweep.json` | Each writer is Python in `cli/src/fno`. About 13 more Python lines to move, over budget. They move into `state/` when their writers port. |
| `.think-spawn-daily.json`, `.a2a-confirmed`, `.active-backlog-nudge` | Python dot-stamps in `cli/src/fno`, same reason: the writers are Python and their move waits on the port. |

## Owned subfolders and remaining root state

Every subfolder and file below was found in the real root unnamed at the 2026-09-08 backfill and given a writer and a lifetime. When the root grows an entry this page does not name, `cli/tests/test_state_root_inventory.py` fails.

| Entry | Writer | Lifetime |
|---|---|---|
| `attention/items.json` | `crates/fno-agents/src/attention_arm.rs` (the `attention` arm) | the attention projection cache plus `questions_dir`, rewritten every beat; the king check-in reads it and refuses when it is missing or over 600 s old, safe to delete, next beat rebuilds it |
| `attention/questions.json` | `crates/fno-agents/src/attention_arm.rs` | page settle state (body hashes and since-stamps); deleting it restarts every settle window and cannot double-deliver, because a page's existence proves delivery |
| `attest/` | `hooks/attest-model.sh`, `hooks/review-hold.sh` | one attestation sidecar per reviewed session |
| `backups/` | `crates/fno-agents/src/graph_store.rs` backup rotation (pruned to `GRAPH_BACKUP_KEEP`), and `cli/src/fno/setup/migrate_paths.py` (`settings.yaml.bak.<ts>`) | graph rotation prunes itself; migration backups are one-shot per install. A backup at most a tenth the size of its predecessor moves that predecessor to `backups/pre-shrink.<name>`, and pins are never pruned. |
| `briefs/` | `paths.briefs_dir()` | permanent sidecar discovery briefs |
| `bus/` | `paths.bus_dir()`, written by `cli/src/fno/bus/` (`messages.jsonl`, `cursors/`) | append-only mail log; each consumer's cursor is overwritten |
| `cache/` | `cli/src/fno/pr/_cache.py` (`cache/pr-status`), `cli/src/fno/king/drain_cache.py` (`cache/king-drain.json`), `crates/fno/src/model_catalog.rs` (`cache/models-dev.json`) | regenerated PR-status cache; king-drain counts keyed on graph stat identity, rewritten per fresh drain read; the models.dev catalog cache, refreshed on a composer open when older than 24 h, safe to delete |
| `events.jsonl.ephemeral` | retired. Ephemeral-class rows commit to the store with `retention_class = 'ephemeral'` and expire at the schema floor | no new writes |
| `events.jsonl.shell-writers.d/` | retired. The shell writer makes one native store commit; no writer-liveness markers exist | no new writes |
| `failover-state.json`, `.lock` | `cli/src/fno/adapters/providers/failover.py`, `runtime_state.py` | permanent breaker state: storm-cap and no-swap-back phases |
| `db/` | the graph and archive stores and their kin: `crates/fno-agents/src/backlog/` (`graph.db`, schema in `mod.rs`, one owning module per aggregate; reached from the `paths.graph_json()` anchor through `docs/state-root-layout.tsv`'s `place` resolver, the legacy root spelling still readable), the archive store (`graph-archive.db`, populated by the operator's import; `read_archive` in `graph_store.rs` reads the projection), `graph_store.rs::BoundedLock` (`graph.json.lock`, the publish cycle's bounded lock), `graph_keeper.rs::store_socket_for` (each store's `*.store.sock` IPC socket and its bounded `.lock`, server-managed, unlinked by the keeper on exit and the daemon's `store_socket_sweep`), `backlog/note_history.rs::history_path` (`graph.db.history/notes.jsonl`: PERMANENT node-prose history, every replaced or cleared `current_state` pre-image and every evacuated note, append-only, hash-verified, never rotates; the only copy of evacuated prose, safe to copy with the graph, fatal to delete), `fno doctor graph export --now` (`graph.json`, the on-demand JSON snapshot), `paths.relatedness_json()` (`relatedness.json`, regenerated), `cli/src/fno/plan/cli.py` (`.plan-sync-watermark-v2`, the sweep's shared gate), `fno-event-store` (`events.db`, `decisions.db`, `questions.db`: the AUTHORITATIVE event stores, schema v2 seq/event_id/retention_class/identity; every writer commits here, every reader queries here; resolved through `place`, and the straggler lane imports a reappeared legacy store by `event_id` with INSERT OR IGNORE, then parks it), `cli/src/fno/approvals/store.py` (`approvals.db`, permanent SQLite store for approvals and effect attempts) | durable row store; WAL sidecars are SQLite-managed |
| `heal/pr-heal.pid` | `crates/fno-agents/src/heal_pid.rs::pid_file` (the pr-heal drive loop, beside the global events journal) | server-managed pid file; unlinked by the loop on a clean exit and by the pid scan when the pid is dead |
| `handoffs/` | `paths.handoffs_dir()` | handoff payloads; `scripts/handoffs-migrate-to-vault.sh` moves aged ones to the vault |
| `install/` | `update.py` (`installed-rev`, `installed-rust-rev`, `source-path`, `source-pin.json`), `hooks/session-start.sh` (`plugin-root`, `.worktree-hook-root`), every reader resolving through the layout table (`crates/fno-agents/src/state_layout.rs`, `crates/fno/src/state_layout.rs`) | permanent install markers; the state-root migration moved them off the root (law d-8ddaba56), and `fno-agents state migrate` keeps the legacy names readable until it runs |
| `backups/state-root-migration/<stamp>/` | `crates/fno-agents/src/state_layout.rs::migrate` | parked legacy copies from the state-root migration, one stamp folder per apply run; the layout table's park rows and every conflict-parked legacy file land here. Never swept; deletion requires a recovery receipt with zero missing event identities and the operator's yes |
| `backups/state-recovery/<digest>/` | `crates/fno/src/state_recovery.rs::apply` | verified SQLite snapshots, consumer sidecar snapshots and the immutable recovery batch manifest. Retain until the operator approves a separate rollback or removal. |
| `inbox/` | `paths.inbox_agents_root()` (`cli/src/fno/paths.py`), the mail bus's fallback root: one mailbox per agent handle under `agents/` | mail drains per handle; a drained envelope is acked away |
| `.interrupted-writes/` | `crates/fno-agents/src/daemon.rs` (quarantine) | writes caught mid-flight; released after the write settles |
| `logs/` | `cli/src/fno/agents/mux_spawn.py` (spawn logs), `pr_watch/_install.py` and `backlog/groom.py` (the launchd plist `StandardOutPath`/`StandardErrorPath`), `hooks/git-protection.py` (`logs/merge-gate-overrides.log`), the corrections hook family (`logs/corrections.log`, resolved through `scripts/lib/corrections-lock.sh` and `crates/fno-agents/src/finalize.rs::corrections_log_path`) | unrotated spawn logs plus the moved writers |
| `logs/cargo-fallback-writers.log` | `scripts/lib/cargo-rustc-wrapper.sh` | one line per cargo that builds without the build-dir env; self-trims to its last 500 lines at 1,000 |
| `mail-escalations/` | `cli/src/fno/mail/cli.py` (debounce markers via `O_CREAT|O_EXCL`) | one empty marker per sender/recipient pair inside the debounce window; safe to delete, the next escalation re-creates it |
| `notes/` | `cli/src/fno/research/core.py` (`notes/research`) | permanent research notes |
| `nudge-cursors/` | `cli/src/fno/agents/nudge.py` | one cursor per nudge target, overwritten |
| `announce-cursors/` | `fno-agents announce` (`crates/fno-agents/src/announce.rs`) | one seen-id set per session; pruned to ids still on retained bus segments |
| `law-edit-seen/` | `fno inbox law` edit read (`crates/fno-agents/src/law_match.rs` `edit_answer`, via the shared cursor in `announce.rs`) | one seen-id set per session; tiny, never cleaned |
| `observer-reports/` | the observer fold, via the `paths` accessor | one report per observation run |
| `operator-capture/` | `cli/src/fno/inbox/operator_turns.py` writes `<session-id>.jsonl`, the ack ledger and receipt; `fno-agents compaction operator-turns` (`crates/fno-agents/src/operator_turns.rs`) writes `<session-id>.scan.json` | per session, the ack ledger is permanent. The scan cursor cache is safe to delete, the next read rescans from byte 0 |
| `fleet/` | the transcript fold behind `fno-agents intel --fleet` (`crates/fno-agents/src/transcript_activity.rs`), `activity.json` plus its `.lock` | cursor and hour cache, safe to delete, rebuilt from the window on the next run |
| `postmortems/` | the retro routine and stuck-terminal postmortem writer, via the `paths` accessor | permanent |
| `provider-runtime-state.json`, `.update.lock` | `cli/src/fno/adapters/providers/runtime_state.py` via the `paths` accessor | permanent; the update lock lives for one write |
| `providers/` | `cli/src/fno/adapters/providers/managed.py`, `staging.py` | permanent managed provider configs |
| `cargo-build/<h2>/<hash>/` | cargo itself, via the tracked `build.build-dir` and the `CARGO_BUILD_BUILD_DIR` value `fno.paths.cargo_build_dir_value()` exports | per-workspace intermediates, one hash dir per workspace root; final binaries stay in the checkout's `crates/*/target`; reclaimed by `worktree cleanup --cargo-targets` (live workspaces' hash dirs are protected via `cargo metadata`) |
| `plugin-stage/fno/` | `fno config plugin install` via the `fno-agents plugin-install` verb (`crates/fno-agents/src/plugin_install.rs::build_stage`); restaged by `fno doctor update` whenever the directory exists | a filtered copy of the checkout (git-tracked + untracked-but-not-ignored files only, no target/, worktrees or venvs); rebuilt wholesale on every install, so nothing in it outlives the next one; the swap is atomic by rename and hook scripts the old stage's config references are carried forward; `fno-agents plugin-install --check` is the drift verdict and `fno doctor` exits 1 on a stale stage |
| `reclaim/last-run.json` | `fno doctor reclaim` via `crates/fno-agents/src/reclaim.rs::write_receipt` (the Python doctor verb shells to `fno-agents reclaim`) | overwritten per `--apply` run; records bytes reclaimed per lane; the daemon's daily sweep gates on its mtime |
| `push-stamps/` | `hooks/git-protection.py` | one stamp per protected push |
| `relay-claude/` | Claude Code itself, via a `CLAUDE_CONFIG_DIR` account alias | operator-managed harness home. Never fno state, never swept. |
| `retro-pending/` | `paths.retro_pending_dir()`, written by `cli/src/fno/retro/sweep.py` | per-PR retro evidence awaiting harvest |
| `review-invocations/` | `cli/src/fno/review/invocation.py`, `crates/fno-agents/src/codex_inject.rs` | one invocation record per review round |
| `saved-sessions/` | `scripts/save-session.py` | one transcript per saved session, written on demand |
| `sessions/<project-slug>/` | `crates/fno-agents/src/footnote_harness/transcript.rs` | one record + sidecar per `-H footnote` session, every file id-named: `<fno_id>.jsonl` (the record, beside the dir), `<fno_id>/` holding `<fno_id>.lock`, `<fno_id>.index.db` (journal `<fno_id>.index.jsonl`), `<tool_call_uid>.out` spill, `<fno_id>.diag.log`. The slug is the canonical checkout's space slug, `_none` outside a repo. Permanent; never holds credentials |
| `spaces/` | the project-space layout (`cli/src/fno/paths.py`, `crates/fno-agents/src/state_path.rs`) | permanent; detailed in the project-space section below |
| `worktree-salvage/` | `hooks/worktree-salvage-ref.sh`, `scripts/setup/setup-worktree.sh` | salvage-mirror state per worktree |
| `state/` | `hooks/git-protection.py` (`state/git-protection.json`), `hooks/buddy/register.ts` (`state/buddy/`: the status line wrapper copy, the saved `statusLine` in `inner.json`, and per-session `frames/<id>.json` plus `.seen`, rewritten while a session draws), `crates/fno-agents/src/operator_notice.rs` (`state/notify-signals.json` via `place`), `hooks/worktree-peers-session-start.sh` (`state/.worktree-stranded-cache.json` + refresh stamp in the ambient branch), `crates/fno-agents/src/machine_watch.rs` (`state/machine-brake.json` via `place`; spawn admission reads the same table row in `crates/fno/src/process_admission.rs`) | rewritten runtime state; newer bytes win, and every value here is safe to delete (the next write rebuilds it) |
| `history/` | `paths.evals_history()` (`history/evals-history.jsonl`), `health_monitor.py` and `graph/triage.py` (`history/health-history.jsonl`), `think_inspect.py` and `scripts/memory/append-lesson-candidate.sh` (`history/lesson-candidates.jsonl`) | append-only jsonl journals |
| `pages/` | `graph/_constants.py` (`pages/graph.md`, `pages/graph.html`), `cli/src/fno/king/ledger.py` (`pages/reign.html`, `pages/rundown.html` via the org-title rename), `crates/fno-agents/src/fleet_page.rs` (`pages/fleet.html` via `place`), served back by `crates/fno/src/web.rs` through the same resolver | regenerated per render; the web bridge reads the same paths |
| `worktrees/` | `fno agents workspace worktree ensure` under the worktree policy (`.claude/rules/worktrees.md`) | fno-managed external worktree base, `<repo>/<name>`; reaped on merge |
| `board.py`, `board.sh`, `board_ids.py`, `board_render.py` | the operator, by hand (not repo files; the `my-priorities.md` row above already names `board.py`) | permanent operator tools |
| `validity-decks/` | `cli/src/fno/graph/maintain.py::write_validity_deck` (via `graph/cli.py`) | one deck per validity run, keyed by timestamp and node; permanent record |
| `.env` | the operator, by hand: the file's own header says it holds global fno secrets (model-routing keys) | until the operator rotates the key. Never echo its contents into a log, a receipt, a fixture, or a PR body, and read it only when the task requires it. |

## Machine and foreign junk

Not written by anything in this repo. Named so the gate can tell known junk from new drift.

| Entry | Writer | Lifetime |
|---|---|---|
| `.DS_Store`, `.metadata_never_index` | macOS Finder and Spotlight | regenerates on view; safe to delete |
| `.claude`, `.fno`, `.impeccable` | foreign plugins and nested workspaces whose cwd was the state root | leave in place, per the foreign-debris section below |

## Unclassified (follow-up filed)

Present in the real root, no writer found, not confirmable as dead inside this task's budget. The gate allowlists exactly these names. A follow-up node owns the verdict.

| Entry | What is known | Follow-up |
|---|---|---|
| `tailscale-migration/` | Operator's 2026-08-22 tailscale migration scripts and status snapshot. Infrastructure work, not an fno writer. | follow-up node "Rule on two operator-owned state-root leftovers", filed 2026-09-08 |
| `tools/` | One operator script, `wake.sh` (2026-08-15). Not written by the repo. | follow-up node "Rule on two operator-owned state-root leftovers", filed 2026-09-08 |

## Unrotated logs

Real writers, no rotation, no deleter. `ledger.md` stays at the root for now (a Python locator awaiting its port). The pr-watcher and groom logs moved under `logs/`. Their rotation is still unclaimed work.

| Entry | Writer | State |
|---|---|---|
| `ledger.md` | `cost/_register.py` | append-only, ~1 MB and growing |
| `logs/pr-watcher.out.log`, `logs/pr-watcher.err.log` | `pr_watch/_install.py` | unbounded |
| `logs/groom.out.log`, `logs/groom.err.log` | `backlog/groom.py` | unbounded |

## Session-scoped state

One file per session, per day, or per throttle window.

| Entry | Writer | Lifetime |
|---|---|---|
| `latches/.context-nudge-*` | `hooks/context-nudge.sh` | pruned by the same hook at `-mtime +2` |
| `latches/.worktree-create-<session-id>` | `hooks/worktree-setup.sh` (and its /speculate copy) | create-attempt counter, deleted by the same hook on a successful create, stale copies pruned at `-mtime +2` |
| `.a2a-confirmed` | `agents/dispatch.py` | single file, overwritten |
| `.active-backlog-nudge` | `active_backlog.py`, `crates/fno-agents/src/active_backlog.rs` | single file |
| `.think-spawn-daily.json` | `provenance/spawn_think.py` | daily, overwritten |
| `.preflight-cancel` | `scripts/ci/preflight.sh` | consumed by the reader (one-shot; stale after one hour) |
| `.path-migration-done` | `setup/migrate_paths.py` | one-shot sentinel |
| `.preflight-receipt-locks/` | `scripts/ci/preflight.sh` | live lock dirs |
| `mail-hold/<handle>.json` | `fno/mail/hold.py` via `paths.state_dir()`, and `crates/fno-agents/src/mail_hold.rs` via its `state_root()` (byte-identical dialect, inventory in `docs/architecture/dual-implementation-inventory.md`) | one file per held session; a clock the conversation rules wrote adds an optional `"source": "conversation"` key; deleted by the release timer (the conversation grace lapses through the same timer), by `fno agents mail hold --off`, and by the turn-boundary tidy in `fno agents mail notify-self` |
| `mail-hold/<handle>.parked/` | `crates/fno-agents/src/mail_hold.rs` (`--park` writes `<ms>-<pid>.txt` payloads, `--run-parked` drains them) | parked raw payloads for one held session, waiting out its hold; each file is deleted by the runner on a sent payload and left in place on a failed send for the next park to pick up; lifetime ends with the hold plus one send |
| `route-settings/<sha16>.json` | `agents/model_routing.py::_write_settings_env_file` via `paths.state_dir()` (content-addressed, 0600, carries a live auth token) | one file per distinct route overlay, shared by every session on that route; a resume re-resolves provider-default tiers (`refresh_provider_default_tiers`), so a moved default yields a new file rather than a served stale one; files no registry row references and older than 14 days are pruned by `fno config route settings ls --prune` |
| `flight/<encoded key>.json` | `crates/fno-agents/src/single_flight.rs`, beside the claims dir it locks in (`$FNO_CLAIMS_ROOT`, else `$HOME`) | one file per distinct fno invocation; rewritten by each flight and read only inside the freshness window (`config.agents.single_flight_ttl_seconds`, default 10 s), so a leftover is inert rather than stale. Pruned past `ttl + join budget`, doubled, by `single_flight::prune_records` on the daemon's GC tick. Holds the child's stdout, which is the same text the verb prints on a terminal |
| `locks/github-graphql-quota.lock` | `pr/_quota.py` via `paths.graphql_quota_lock()` | permanent empty sidecar; flock lives only for the probe-plus-command critical section |
| `locks/github-request-budget.json` (+ `.lock`) | `crates/fno-agents/src/gh_budget.rs` via the `fleet-incident` action's `gh-budget` argument | rewritten per admitted request; stamps pruned past 60 s |
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

An unexpected plugin or project-state directory nested *inside* the state root is not a root writer. Each holds project-relative paths written by a process whose working directory happened to be the state root. When `FNO_REPO_ROOT` is unset and `git rev-parse` fails, `paths.resolve_repo_root()` falls back to `Path.cwd()`. Foreign plugins do the same with their own literals.

Leave them. The finding is the cwd fallback, not the directories it produced.

## The project journal and `FNO_EVENTS_PATH`

The per-repository journal `<space>/events.jsonl` resolves through `paths.project_events_json()`. `FNO_EVENTS_PATH` overrides it. The override exists because repo-root resolution cannot be sandboxed. `fno.hermetic.neutralise` deliberately leaves `FNO_REPO_ROOT` unset. So an unpathed `append_event` under test writes a real row into the developer's space, and both operator readers fold that file. On 2026-08-17 six test fixtures sat in the needs panel beside two genuine operator questions.

`neutralise` pins the override at one line, and that env reaches the pytest, shell, and cargo trees. All three writers read it: the Python resolver, `scripts/lib/events.sh`, and `claim_events_path` in the Rust claims module. Rust has to read it because the two implementations share that journal and its `.lock.d` mutex as a wire contract. A pin one side ignores splits the writers apart. The loop-journal writers in `fno-agents` still build their path by hand and remain outside the pin. Reach for the pin, not for a marker the fold recognises. A fold that must know about test data carries an exception list, and the next fixture that misses the list refills the queue in silence. A test that sets `FNO_REPO_ROOT` itself and reads the journal back must name the same file in `FNO_EVENTS_PATH`. The pin outranks the root.

## Adding a new root writer

You do not. The root rows are shrink-only (see The rule). The runtime guard refuses a new top-level write, and the CI gate fails a PR that adds a row. The remedy for a new surface is a subfolder:

1. Put the state in a named subfolder. Reach for the root only through a row that already exists.
2. Add the accessor to `cli/src/fno/paths.py` so the location follows `config.state_dir`. When a bash caller needs it, export it from `cli/src/fno/setup/emit_shell.py`, then regenerate `scripts/lib/paths.sh`.
3. Name the deleter. Ephemeral state gets its lifetime in the code that writes it, not in a separate janitor. A janitor drifts from the writer and goes unrun. `scripts/prune-fno-dir.sh` was deleted for exactly that: never once invoked, while every file on its delete list sat in the root.
4. If a row above became wrong, fix or delete that row in the same PR. Shrinking is the one direction the gate allows.

## The project space (`~/.fno/spaces/<slug>/`)

Project state left the checkout. One space per repository, keyed on the CANONICAL repo root (the git common dir's checkout), slug = the canonical path with `/` swapped for `-` (Claude's project-dir shape: read the directory name, see the path). Every worktree of a repo resolves to ONE space, so cross-worktree state needs no symlink. Per-worktree state sits at `<space>/worktrees/<worktree basename>/`. A checkout keeps only `.fno/config.toml` (committed project config) and the sandbox breadcrumb below. The first resolve of a moved file renames the legacy `<repo>/.fno/<file>` into the space and leaves a `<repo>/.fno/MOVED-TO` pointer naming it.

| Entry | Writer | Lifetime |
|---|---|---|
| `<space>/events.jsonl` | `paths.project_events_json()`; legacy bytes only since the event-store cutover | import source |
| `<space>/events.db`, `.db-wal`, `.db-shm` | the `fno-event-store` crate, the authoritative event store beside each journal | durable and gate rows forever, ephemeral 672 h |
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
| `<space>/scratchpad-adjacent diagnostics`: `loop-check.stderr.log`, `finalize.stderr.log`, `.loop-check-unavail-*`, `.king-resolve-unavail-*`, `.think-offer-cursor` | `hooks/target-stop-hook.sh`, `hooks/footnote-agy-target-stop-hook.sh`, `hooks/born-with-why-offer-inject.sh` | bounded retries/diagnostics; counters self-heal on the first clean decision |
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

## Recover parked history

Run `fno doctor event recover --root ~/.fno` for a read-only audit. The command reports each parked store, missing identities, conflicts and a packet digest. Unsupported schemas or changing sources refuse recovery. It never removes a backup.

Event families use `event_id` rather than the source sequence. Recovery preserves every non-sequence value and assigns destination sequences. A `recovery_history` ledger commits with each imported family. Historical reads remain visible. Activity readers suppress recovered rows without advancing across genuine concurrent controls.

For copied stores, use `--apply --copy-proof --packet-digest <digest>`. The physical root must be under an OS temporary directory. Each destination must have a different inode from its live counterpart. Pass copied consumer files with repeatable `--sidecar <path>` arguments. Sidecar bytes are captured before the audit, so a later genuine edit differs from its recovery baseline.

Live apply requires `--apply --packet-digest <digest> --packet <file> --approval <decision-id>`. The packet names the absolute root, audit digest, zero historical effects, genuine controls handled once, consumer inventory, verified artifacts and consumer sidecars. An active operator law must state `approve state recovery <absolute-root> <audit-digest> <packet-file-sha256>`. Agent or crown coordination cannot grant apply. A worktree binary cannot apply to operator stores.

Before any family writes, recovery snapshots all five stores through SQLite's backup API and checks integrity. Snapshot receipts include hashes, inode identities, schema versions, row counts and sequence high-water marks. Required sidecar failures abort. Resuming the same packet verifies its original sources and snapshots, then imports only its remaining approved identities. Conflicting live identities refuse instead of overwriting concurrent writes.

Include existing question pages in the sidecar inventory. Recovery does not deliver a new page for an old ask. An unchanged historical page cannot record an answer. A later page edit remains actionable. A missing or altered baseline holds that page rather than replaying its old tick.

Approvals and archive stores are audited by owning primary keys. Derived archive FTS tables are excluded from identity counts. Missing owning rows in either family hold apply until their replay-safe recovery exists. `zero_missing` requires every audited family to be complete and readable.

Keep migration backups, recovery snapshots and the ledger. Pruning needs a fresh zero-missing audit and separate operator approval. Never restore a snapshot over live stores automatically. Such a restore can discard intervening writes. Rollback requires an approved identity-based plan that accounts for those writes.

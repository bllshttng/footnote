# Process-backed stress suite audit

## Baseline

Baseline SHA: `bcb46735f595e0d85f817b64739d9a8b57037ef5`. It was `origin/main` before the follow-up fast-forwards. Those fast-forwards changed no test files listed below.

The three trial baseline passed with `STRESS_TRIALS=3`: `daemon_e2e=pass`, `persistence=pass`, `workspace_persistence_e2e=pass`, and `daemons_left=0` on all three trials.

Baseline support line counts: the stress harness had 225 lines. The daemon test had 2,783 lines. The persistence test had 777 lines. The workspace test had 863 lines.

Baseline declarations: daemon had 30. Persistence had 14. Workspace had 9. Total: 53. None was ignored. A shared test-owner helper also runs once in each fno integration binary, adding two cases per stress trial.

## Cost evidence and owner choice

Recent `cli-ci` run 36109172701 measured `smoke-rest (4)` at 1,234 seconds (20.57m). Its stress step ran three trials in 545 seconds (9.08m). Recent `rust-ci` run 36110477448 measured the 20-trial stress job at 1,238 seconds (20.63m). It reported zero failures.

The workflow source records 35.9 seconds per repeated trial across three binaries. Recent runs confirm this suite has high cost. The stress job reports results only. The three-trial smoke lane blocks.

We chose the three process-backed binaries in `stress-rust-e2e-concurrency.sh` from these measurements. The ordinary Rust integration run remains their primary keeper. It still runs every declaration once per relevant PR.

## Owner-boundary ledger

`R` retains an independent contract. `F` retains the contract and repairs a weak assertion. `C` folds a contract into a named keeper and preserves its evidence. No declaration was marked `D`.

### `crates/fno-agents/tests/daemon_e2e.rs`

| Declaration | Mark | Contract and disposition |
|---|:---:|---|
| `cold_start_reconciles_stale_ask_row_to_exited` | R | Startup reconciles stale ask rows while preserving session identity and the reconciliation stamp. |
| `startup_reconcile_failure_degrades_to_serving` | R | Failed startup sweep preserves stored status while the daemon continues serving. |
| `cold_start_serves_while_the_startup_sweep_is_still_running` | R | A real status response arrives before the delayed startup sweep completes. |
| `client_declines_to_spawn_while_the_singleton_lock_is_held` | R | A held singleton lock prevents a competing daemon spawn. |
| `restart_over_a_large_roster_leaves_exactly_one_daemon` | R | Concurrent restart at the observed 28-row failure scale leaves one daemon. |
| `daemon_child_env_isolated_probe` | R | Intended and sibling daemon launches receive distinct child environments; its stdout marker remains the stress vacuity guard. |
| `status_client_exits_13_when_daemon_down` | R | The real status client reports down without lazy-starting a daemon. |
| `a_daemon_restart_over_a_loss_shaped_registry_loses_no_rows` | R | Startup read-modify-write preserves every row in a loss-shaped registry. |
| `a_future_schema_registry_is_refused_not_dropped_on_restart` | R | Startup refuses future-schema data and preserves the original bytes. |
| `daemon_idle_exits_over_terminal_rows_and_says_why` | C | Folded into `daemon_on_sandbox_home_starts_no_active_backlog_supervisor`; preserve two terminal rows, idle shutdown, dead PID, clean exit, and `reason=idle`. |
| `daemon_on_sandbox_home_starts_no_active_backlog_supervisor` | R | Keeper preserves sandbox scope and no active-backlog arm, plus terminal-row idle exit, dead PID, clean exit, and `reason=idle`. |
| `daemon_stays_resident_while_a_worker_socket_is_live` | R | A reachable worker socket keeps a real daemon alive beyond its idle window. |
| `rm_reaps_registry_claude_and_mux_surfaces_in_one_call` | R | Keeper removes registry, Claude, and mux surfaces and records the audit event. |
| `pinned_daemon_sheds_runtime_pin_before_spawning_an_rm_child` | R | Child observes `FNO_AGENTS_RUNTIME=python` is shed before the `rm` subprocess. |
| `stop_keeps_non_pane_noop_receipt` | C | Folded into the pane-refusal keeper; preserve the Codex no-op receipt with `no_op=true` and `stopped=true`. |
| `stop_refuses_a_pane_row_and_names_the_row_ref` | R | Keeper refuses stopping a pane row, names the session/pane and clearing verb, leaves the row live, and preserves the non-pane `no_op=true` and `stopped=true` receipt. |
| `restart_when_down_starts_fresh` | R | Restart with no incumbent creates a live successor and reports `old_pid=None`. |
| `the_default_wrapper_exports_the_test_binary_owner` | R | The no-override wrapper arms its child on the test-binary owner; distinct from explicit-owner watchdog behavior. |
| `a_restart_successor_dies_with_its_test_owner` | R | Killing the owner reaps the daemon successor created by the real wrapper path. |
| `restart_force_recovers_a_wedged_holder` | R | Force restart recovers a SIGSTOP holder and serves from a fresh daemon. |
| `restart_force_refuses_to_signal_a_recycled_pid` | R | Recycled or missing start-token identities are never signalled. |
| `drift_warned_on_list_stderr_only` | R | Keeper preserves clean JSON stdout and a drift warning on stderr through the real client/socket path. |
| `registry_startup_refuses_a_divergent_nonempty_registry` | R | Startup refuses same-schema decode loss before publishing a serving socket. |
| `registry_list_refuses_over_a_broken_registered_lane` | R | Keeper refuses both human and JSON list forms over a broken registered lane. |
| `registry_lookup_distinguishes_unreadable_from_absent` | R | Mail recipient lookup distinguishes an unreadable registry from a missing recipient. |
| `registry_true_empty_registry_still_serves_zero` | R | A genuinely empty registry remains a valid zero-agent state. |
| `registry_runtime_upgrade_refuses_a_partial_roster` | R | A future-schema runtime upgrade refuses the raw=3/decoded=2 partial-decode roster. |
| `cold_start_settles_a_failed_codex_thread_resume_to_orphaned` | R | Startup scheduling settles failed Codex resume state through the real daemon. |
| `status_json_carries_the_drift_label` | C | Folded into the drift-list keeper; assert `status --json` reports `drifted` with valid JSON. |
| `drift_warned_on_list_stderr_only` | R | Keeper preserves clean JSON stdout and a drift warning on stderr, and asserts `status --json` reports `drifted`. |
| `daemon_cwd_survives_a_reaped_launch_worktree` | R | A real daemon retains a valid cwd after its launch worktree is removed. |

Three deterministic daemon waits stay in ordinary Cargo integration coverage. The stress loop skips them after cutover. The environment probe still carries the stress marker.

### `crates/fno/tests/persistence.rs`

| Declaration | Mark | Contract and disposition |
|---|:---:|---|
| `persistence_reattach_restores_the_exact_screen` | R | Real detach/reattach preserves the settled screen. |
| `persistence_alt_screen_program_survives_detach_reattach` | R | A live alternate screen survives real-client detach/reattach. |
| `persistence_multi_pane_reattach_is_screen_exact` | R | Multiple real-client panes redraw exactly after reattach. |
| `persistence_kill_nine_of_the_client_leaves_the_pty_running` | R | SIGKILL without protocol goodbye leaves the shell session attachable. |
| `persistence_dead_server_respawns_fresh_instead_of_hanging` | F | Retain stale-socket recovery, notice, and fresh shell; remove the final negative assertion that duplicates the same `old=[yes]` check inside `wait_screen`. |
| `persistence_two_cold_clients_converge_on_one_server` | R | Two real cold clients converge on one owner and share the session. |
| `persistence_late_loser_is_not_a_second_owner` | R | A delayed real contender remains a loser and proves it actually ran. |
| `persistence_one_owner_catches_a_real_second_owner` | R | Positive control calibrates the real-owner oracle used by the race tests. |
| `persistence_malformed_frame_is_rejected_not_panicked` | R | A malformed real client frame is rejected without panic. |
| `persistence_client_relays_a_version_skew_refusal` | R | The client exposes the server's version-skew refusal before terminal takeover. |
| `persistence_zero_client_session_survives_and_resyncs_fully` | R | An empty-client session remains listed and restores its pane content. |
| `persistence_last_pane_exit_with_zero_clients_ends_the_server` | R | The final pane exit with no clients ends the server and unlinks its socket. |
| `external_lifecycle_round_trips_through_the_production_store` | R | Production lifecycle state survives independent store loads. |
| `build_tree_guard_refuses_a_write_without_agents_home` | R | Retain the production writer boundary because the sibling CLI keeper does not yet carry both no-global-write and redirected-root assertions. |

### `crates/fno/tests/workspace_persistence_e2e.rs`

| Declaration | Mark | Contract and disposition |
|---|:---:|---|
| `old_server_reaped_before_rebind_probe` | R | Stress sentinel proves the old server is reaped before its socket is rebound. |
| `symptom_rename_survives_restart` | C | Folded with `symptom_removed_workspace_stays_removed` into one restart scenario. |
| `symptom_removed_workspace_stays_removed` | C | Folded into the rename keeper; preserve the absence window alongside the old/new names. |
| `symptom_rename_and_removal_survive_restart` | R | Keeper restores the new workspace name, rejects the old name, and keeps a separately removed workspace absent. |
| `symptom_hand_split_survives_restart` | R | Direct stop/restart preserves a two-pane hand-split tab; distinct from the CLI kill-server path. |
| `symptom_kill_server_restores_the_exact_layout` | F | Keeper retains real CLI kill-server/restart and clean-store mtime check; assert restored pane rectangles and focus match the pre-kill layout. |
| `symptom_worker_tab_position_and_pane_id_survive_tab_removal_and_restart` | F | Retain same pane identity and assert `pane ls` locates it in `squad=w` and `tab=crew` after restart. |
| `symptom_kill_server_captures_without_a_dirty_flag` | C | Folded into the exact-layout keeper; preserve clean-store precondition and teardown mtime advance. |
| `symptom_stale_live_row_does_not_respawn_a_dead_worker` | R | A provably dead worker row never spawns `claude attach`; the positive boot control proves the instrument ran. |
| `symptom_restore_rebuilds_no_thread_pane` | F | Keep the no-respawn proof; document that restart may rebuild a shell for topology but must not respawn the thread attach process. |

### Candidate evidence and preservation review

Three read-only boundary passes compared every candidate with production owners, callers, siblings, history, and test routing. They found no lost contract. The C rows move assertions into existing process-backed keepers. The F rows repair weak post-restart checks. No production code or seam was removed.

| Candidate | Failure detected and surviving keeper | Non-test caller and history | Cut unlocked, risk, focused validation |
|---|---|---|---|
| `daemon_e2e::daemon_idle_exits_over_terminal_rows_and_says_why` (C) | Terminal registry rows wrongly pin an idle daemon or the exit reason/clean marker is lost; the sandbox keeper now checks terminal rows, no active-backlog arm, reason, clean payload, and dead PID. | The daemon binary is launched by normal `fno agents` operations; this acceptance was separate because empty-registry retirement did not prove terminal-row retirement. | Removes one duplicate daemon fixture; idle timing remains the risk. Validate `fno doctor test rust --manifest-path crates/fno-agents/Cargo.toml --test daemon_e2e daemon_on_sandbox_home_starts_no_active_backlog_supervisor`. |
| `daemon_e2e::stop_keeps_non_pane_noop_receipt` (C) | A pane refusal regresses into a Codex no-op mutation or receipt changes; the pane-refusal keeper now drives both RPC rows and asserts the second receipt. | `fno agents stop` reaches the daemon stop RPC; the two outcomes used the same registry helper and daemon startup path. | Removes one duplicate daemon fixture; startup reconciliation is disabled to keep the seeded live row. Validate `fno doctor test rust --manifest-path crates/fno-agents/Cargo.toml --test daemon_e2e stop_refuses_a_pane_row_and_names_the_row_ref`. |
| `daemon_e2e::status_json_carries_the_drift_label` (C) | `status --json` loses the drift field while list output remains valid; the drift-list keeper now exercises status against the same replaced daemon binary. | `fno agents status --json` is the user-facing client path; the removed case inspected status JSON separately from list's stderr/stdout contract. | Removes a second daemon fixture and copy; binary replacement is the risk. Validate `fno doctor test rust --manifest-path crates/fno-agents/Cargo.toml --test daemon_e2e drift_warned_on_list_stderr_only`. |
| `workspace_persistence_e2e::symptom_rename_survives_restart` and `::symptom_removed_workspace_stays_removed` (C) | Rename reverts or a separately removed workspace resurrects; the combined keeper asserts new name present, old name absent, and removed name absent after the same restart window. | `fno mux` rename/remove commands are the real path; both scenarios used the same server, fake client, and restart harness. | Removes one complete restart fixture; persistence identity is the risk. Validate `fno doctor test rust --manifest-path crates/fno/Cargo.toml --test workspace_persistence_e2e symptom_rename_and_removal_survive_restart`. |
| `workspace_persistence_e2e::symptom_kill_server_captures_without_a_dirty_flag` (C) | A clean topology is not written at shutdown; the exact-layout keeper retains the settled mtime, teardown-advance assertion, and restart. The preservation probe showed a pending debounce could mask the shutdown write, so the keeper settles it before taking the mtime baseline. | `fno mux kill-server` is the real shutdown path; both cases shared the same three-pane setup. | Removes one complete kill/restart fixture; mtime granularity is the risk. Validate `fno doctor test rust --manifest-path crates/fno/Cargo.toml --test workspace_persistence_e2e symptom_kill_server_restores_the_exact_layout`. |
| `workspace_persistence_e2e::symptom_kill_server_restores_the_exact_layout` (F) | Pane count stays three while restored geometry or focus changes; keeper selects workspace `w` on both sides, focuses a non-first pane, and compares pane rectangles and focus. | The attached client receives real server layouts; the old assertion only checked pane count and tab name. | No seam removed; changed focus or workspace selection is the risk. Validate with the same exact-layout command above. |
| `workspace_persistence_e2e::symptom_worker_tab_position_and_pane_id_survive_tab_removal_and_restart` (F) | Pane id survives but the worker lands in the wrong workspace or tab; `pane ls` now must report the restored `w` squad ID and `tab=crew`. | `fno mux pane ls` is the real operator listing; the old positive control asserted only pane identity/name. | No seam removed; pane-list formatting is the risk. Validate `fno doctor test rust --manifest-path crates/fno/Cargo.toml --test workspace_persistence_e2e symptom_worker_tab_position_and_pane_id_survive_tab_removal_and_restart`. |

## `tests/spec` route inventory

Before campaign one merged, 17 Python tests passed in 17.39s on the same SHA. The executor shell spec passed 23 assertions. Blueprint phase-close passed. Claims stopped at preflight with two failures because the old templates were gone.

After campaign one merged, the main baseline had 16 Python cases pass in 19.00s. Claims passed 12 assertions. Executor passed 17 assertions. Blueprint phase-close passed. `scripts/lib/parse-claims-arg.sh` owns the parser fix. The error-output regression stays.

`scripts/tests/test-spec-suite.sh` is auto-discovered by the smoke runner. It runs all three Bash specs and both Python files through `fno doctor test`.

## Cutover result

Cutover leaves 48 process-backed declarations. Baseline had 53 (30 daemon, 14 persistence, 9 workspace). The three-trial `cli-ci` and 20-trial report-only `rust-ci` samples run 48 cases per trial. Each trial includes two shared test-owner cases and 46 audited declarations. Six old declarations were retired. The rename and removal checks now share one new keeper. Two deterministic daemon tests are skipped only in repeated samples. Ordinary Rust integration runs all 50 cases.

The three process test files shrink from 4,423 to 4,387 lines. That is 36 fewer lines. The stress harness grows from 225 to 240 lines. The new spec router has 9 lines. Rust production changes add 0 lines. The Python runner change only updates comments.

Recent stress samples cost 545s for the three-trial step and 1,238s for the 20-trial report-only job. Their combined cost was 29.72 runner-minutes. Three daemon waits cost about 13s per baseline trial. Consolidation and stress-only skips remove those waits. The new route adds about 15s to smoke. Projected affected-step cost is 1,499s (24.98 runner-minutes). That saves 4.73m. This is an estimate because we will not wait for PR CI.

The stress loop executed 1,265 cases across 23 baseline trials. Cutover will execute 1,104. That is 161 fewer repeated cases. CI gains three Bash specs and 16 Python cases. They were not routed before. `scripts/tests/test-spec-suite.sh` passed the smoke registry. Claims passed 12/12. Executor passed 17/17. Blueprint phase-close passed. Pytest passed 16/16. The smoke shard wiring test passed 1/1. The post-merge full run passed one trial: 27 daemon, 15 persistence, and 8 workspace cases, with no daemon leaks.

Mutation proof: 8/8 contracts triggered their keeper assertion after an owner mutation. The runner restored each source file to its original SHA. Mutations covered idle exit, no-op receipt, status drift, rename, removal, clean shutdown capture, restored focus, and worker tab location.

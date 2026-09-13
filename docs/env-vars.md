# Environment variable registry

Every env var name either tree reads, one row each. The registry IS this table. `scripts/ci/check_env_registry.py` fails a read with no row, a row with no reader, and a wrong Read by. `--update` regenerates the table and preserves Meaning text.

A meaning not derivable from the read site stays `unclear: <file:line>`, never invented. A name passed through a variable is not seen (scanner limit).

| Name | Read by | Meaning |
|------|---------|---------|
| `ANTHROPIC_API_KEY` | py | Anthropic API key; presence enables bare-key auth for the LLM lane. |
| `ANTHROPIC_BASE_URL` | py+rs | Overrides the Anthropic API base URL. |
| `ANTHROPIC_MODEL` | py | Overrides the default Anthropic model. |
| `CARGO_HOME` | py+rs | Cargo install root; the default is ~/.cargo. |
| `CENSUS_DEFERRED_FILE` | py | unclear: cli/src/fno/test_cmd.py:2250 |
| `CENSUS_KILL_BOUND_S` | py | unclear: cli/src/fno/test_cmd.py:2297 |
| `CI` | py+rs | unclear: cli/src/fno/llm.py:40 |
| `CLAUDECODE_SESSION_ID` | py | unclear: cli/src/fno/adapters/hermes.py:141 |
| `CLAUDE_CLI` | rs | unclear: crates/fno-agents/src/loop_dispatch.rs:186 |
| `CLAUDE_CODE_SESSION_ID` | py+rs | unclear: cli/src/fno/carveout/core.py:202 |
| `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` | rs | unclear: crates/fno-agents/src/loopcheck.rs:9010 |
| `CLAUDE_CONFIG_DIR` | py+rs | Overrides the Claude config directory for managed provider lookups. |
| `CLAUDE_EFFORT` | py | unclear: cli/src/fno/graph/cli.py:903 |
| `CLAUDE_PLUGIN_ROOT` | py | unclear: cli/src/fno/context_observation.py:93 |
| `CLAUDE_SESSION_ID` | rs | unclear: crates/fno-agents/src/claims.rs:3690 |
| `CLI` | rs | unclear: crates/fno-agents/src/loop_dispatch.rs:198 |
| `CODEX_HOME` | py+rs | unclear: cli/src/fno/adapters/providers/managed.py:211 |
| `CODEX_PLUGIN_ROOT` | py | unclear: cli/src/fno/agent/state.py:143 |
| `CODEX_SESSION_ID` | py | unclear: cli/src/fno/adapters/hermes.py:142 |
| `COLORTERM` | rs | unclear: crates/fno/src/mux_cli.rs:1508 |
| `CRON_JOB` | py | unclear: cli/src/fno/agents/context.py:94 |
| `DATABASE_URL` | py | unclear: cli/src/fno/codemap_cli/db-schema.py:208 |
| `EVENTS_FILE` | rs | unclear: crates/fno-agents/src/verify_evidence.rs:932 |
| `FNO_A2A_NO_CONFIRM` | py | unclear: cli/src/fno/agents/dispatch.py:5376 |
| `FNO_AGENTS_BIN` | rs | unclear: crates/fno/src/server/agent_actions.rs:692 |
| `FNO_AGENTS_DAEMON_BIN` | rs | unclear: crates/fno-agents/src/client.rs:125 |
| `FNO_AGENTS_FAIL_STARTUP_RECONCILE` | rs | unclear: crates/fno-agents/src/daemon.rs:2093 |
| `FNO_AGENTS_FIXTURES` | rs | Overrides the scratch sweep fixtures directory. |
| `FNO_AGENTS_FRONT` | py+rs | unclear: cli/src/fno/graph/store.py:213 |
| `FNO_AGENTS_HOME` | py+rs | unclear: cli/src/fno/agents/cli.py:801 |
| `FNO_AGENTS_IDLE_EXIT_SECS` | rs | unclear: crates/fno-agents/src/bin/daemon.rs:67 |
| `FNO_AGENTS_NO_STARTUP_RECONCILE` | rs | unclear: crates/fno-agents/src/bin/daemon.rs:87 |
| `FNO_AGENTS_RESPONSE_DEADLINE_MS` | rs | unclear: crates/fno-agents/src/client.rs:68 |
| `FNO_AGENTS_RUNTIME` | py | unclear: cli/src/fno/doctor.py:564 |
| `FNO_AGENTS_STARTUP_RECONCILE_DELAY_MS` | rs | unclear: crates/fno-agents/src/daemon.rs:2082 |
| `FNO_AGENTS_WORKER` | py+rs | Marks the process as a footnote worker. |
| `FNO_AGENTS_WORKER_BIN` | py+rs | unclear: cli/src/fno/agents/dispatch.py:888 |
| `FNO_AGENT_HARNESS` | py | unclear: cli/src/fno/harness_identity.py:77 |
| `FNO_AGENT_ROW_PENDING` | py | unclear: cli/src/fno/agents/register_session.py:200 |
| `FNO_AGENT_SELF` | py+rs | unclear: cli/src/fno/agent/cli.py:363 |
| `FNO_AGENT_SESSION` | py | unclear: cli/src/fno/agents/context.py:237 |
| `FNO_AUTO_MEMORY_DIR` | py | unclear: cli/src/fno/inbox/drain.py:407 |
| `FNO_BG` | py | unclear: cli/src/fno/target/orient.py:254 |
| `FNO_BIN` | py+rs | Overrides the Python fno porcelain path at the Rust/Python seam. |
| `FNO_BOARD_SCOPE` | rs | unclear: crates/fno/src/backlog_view.rs:333 |
| `FNO_BOOTSTRAP_WHEEL` | rs | unclear: crates/fno/src/bootstrap.rs:280 |
| `FNO_BUS_DIR` | py+rs | unclear: cli/src/fno/paths.py:1204 |
| `FNO_BUS_MAX_BYTES` | py | unclear: cli/src/fno/bus/log.py:50 |
| `FNO_BUS_RETAIN` | py | unclear: cli/src/fno/bus/log.py:62 |
| `FNO_CAPABILITY_PARITY_DIR` | rs | unclear: crates/fno/src/agents_view.rs:3316 |
| `FNO_CAPABILITY_PARITY_JSON` | rs | unclear: crates/fno/src/agents_view.rs:3318 |
| `FNO_CC_DAEMON_RV_ROOT` | py | unclear: cli/src/fno/agents/session_procs.py:40 |
| `FNO_CLAIMS_ROOT` | py+rs | unclear: cli/src/fno/agents/account_env.py:158 |
| `FNO_CLAUDE_DAEMON_DIR` | py+rs | unclear: cli/src/fno/agents/discover.py:2353 |
| `FNO_CODEX_ASK_WAIT_MS` | rs | unclear: crates/fno-agents/src/codex_thread.rs:56 |
| `FNO_CODEX_INTERRUPT_BOUND_MS` | rs | unclear: crates/fno-agents/src/codex_thread.rs:80 |
| `FNO_CONFIG` | py+rs | unclear: cli/src/fno/adapters/providers/loader.py:436 |
| `FNO_CONFIG_SEARCH_ROOT` | py | unclear: cli/src/fno/config_io.py:66 |
| `FNO_CONTEXT_OBSERVATION_DIR` | py | unclear: cli/src/fno/context_observation.py:120 |
| `FNO_CONTROL_PLANE_SCHEDULER` | py | unclear: cli/src/fno/control_plane.py:22 |
| `FNO_CURSOR_AGENT_MODEL` | py+rs | unclear: cli/src/fno/agents/harnesses/cursor_agent.py:326 |
| `FNO_CURSOR_AGENT_PROVIDER` | py+rs | unclear: cli/src/fno/agents/harnesses/cursor_agent.py:321 |
| `FNO_DEBUG` | py | unclear: cli/src/fno/agents/mux_spawn.py:1854 |
| `FNO_DISPATCH_ACCOUNT_ENV` | py | unclear: cli/src/fno/agents/cli.py:1905 |
| `FNO_DRIVER_LIB` | rs | unclear: crates/fno-agents/src/finalize.rs:849 |
| `FNO_DRIVER_LIB_DIR` | rs | unclear: crates/fno-agents/src/loop_target.rs:616 |
| `FNO_E2E` | rs | unclear: crates/fno/src/client.rs:539 |
| `FNO_E2E_CORE_WEDGE` | rs | unclear: crates/fno/src/server.rs:14343 |
| `FNO_E2E_DROP_PTY_EXIT` | rs | unclear: crates/fno/src/pty.rs:1862 |
| `FNO_E2E_PTY_OUTPUT_DELAY_MS` | rs | unclear: crates/fno/src/pty.rs:1873 |
| `FNO_EVENTS_PATH` | py+rs | unclear: cli/src/fno/agents/spawn_defaults.py:1830 |
| `FNO_GLOBAL_SETTINGS_PATH` | py+rs | unclear: cli/src/fno/adapters/providers/loader.py:48 |
| `FNO_GRAPH_JSON` | rs | unclear: crates/fno/src/backlog_view.rs:47 |
| `FNO_HARNESS` | py | unclear: cli/src/fno/king/state.py:268 |
| `FNO_HEALTH_HISTORY` | py | unclear: cli/src/fno/graph/triage.py:2038 |
| `FNO_HOME` | py+rs | unclear: cli/src/fno/paths.py:1723 |
| `FNO_IDLE_EXIT_GRACE_MS` | rs | unclear: crates/fno/src/server.rs:14324 |
| `FNO_INBOX_ROOT` | py+rs | unclear: cli/src/fno/inbox/store.py:218 |
| `FNO_KILLCHECK_GIT_BIN` | rs | unclear: crates/fno-agents/src/kill_criteria.rs:58 |
| `FNO_LAUNCH_ACCOUNT` | py | unclear: cli/src/fno/agents/rust_runtime.py:1204 |
| `FNO_LLM_STUB` | py | unclear: cli/src/fno/llm.py:38 |
| `FNO_LOOPCHECK_FNO_BIN` | rs | Overrides the fno path the loopcheck shim calls. |
| `FNO_LOOPCHECK_GH_BIN` | rs | unclear: crates/fno-agents/src/loopcheck.rs:7891 |
| `FNO_LOOPCHECK_GIT_BIN` | rs | unclear: crates/fno-agents/src/loopcheck.rs:7892 |
| `FNO_LOOPCHECK_MIN_FIRE_GAP_SECS` | rs | unclear: crates/fno-agents/src/loopcheck.rs:7447 |
| `FNO_LOOPCHECK_NO_COMMENT` | rs | unclear: crates/fno-agents/src/loopcheck.rs:3533 |
| `FNO_LOOPCHECK_NO_NOTIFY` | rs | unclear: crates/fno-agents/src/loopcheck.rs:3514 |
| `FNO_LOOPCHECK_READ_TIMEOUT_MS` | rs | unclear: crates/fno-agents/src/loopcheck.rs:7900 |
| `FNO_MCP_SIDECAR_LOG` | py | unclear: cli/src/fno/mcp/sidecar.py:646 |
| `FNO_MUX_ADMISSION_NAMESPACE` | rs | unclear: crates/fno/src/process_admission.rs:733 |
| `FNO_MUX_DIR` | rs | unclear: crates/fno/src/mux_cli.rs:1533 |
| `FNO_MUX_MOUSE_TRACE` | rs | unclear: crates/fno/src/client.rs:11590 |
| `FNO_MUX_NATIVE_TEST_ADMISSION` | rs | unclear: crates/fno/src/process_admission.rs:416 |
| `FNO_MUX_OPENPTY_HANG_MS` | rs | unclear: crates/fno/src/pty.rs:1355 |
| `FNO_MUX_OPENPTY_TIMEOUT_MS` | rs | unclear: crates/fno/src/pty.rs:1341 |
| `FNO_MUX_PANE_GROUP_MAX` | rs | unclear: crates/fno/src/process_admission.rs:378 |
| `FNO_MUX_SCROLLBACK_LINES` | rs | unclear: crates/fno/src/vt.rs:68 |
| `FNO_MUX_SESSION` | rs | unclear: crates/fno/src/server_stats.rs:33 |
| `FNO_MUX_SHELL_INTEGRATION` | rs | unclear: crates/fno/src/client.rs:577 |
| `FNO_NODE` | py+rs | unclear: cli/src/fno/agents/harnesses/claude.py:725 |
| `FNO_NODE_CLAIM_HOLDER` | py | unclear: cli/src/fno/graph/_session.py:556 |
| `FNO_NOTIFY_SIGNALS` | rs | unclear: crates/fno-agents/src/operator_notice.rs:82 |
| `FNO_NO_CANONICAL_CONFIG` | py+rs | unclear: cli/src/fno/config/__init__.py:4677 |
| `FNO_NO_OPEN` | py | unclear: cli/src/fno/graph/cli.py:6158 |
| `FNO_NUDGE_DISABLED` | rs | unclear: crates/fno-agents/src/nudge.rs:32 |
| `FNO_OBSERVER_GH_CAP` | py | unclear: cli/src/fno/observer/cli.py:184 |
| `FNO_OBSERVER_PR_LIST_LIMIT` | py | unclear: cli/src/fno/observer/cli.py:456 |
| `FNO_OPENCODE_LIVE_TOKEN` | rs | unclear: crates/fno-agents/src/opencode_archive_tests.rs:340 |
| `FNO_OPENCODE_LIVE_URL` | rs | unclear: crates/fno-agents/src/opencode_archive_tests.rs:338 |
| `FNO_OPERATOR_CAPTURE_DIR` | py | unclear: cli/src/fno/inbox/operator_turns.py:87 |
| `FNO_OPERATOR_HARNESS` | py | unclear: cli/src/fno/inbox/operator_turns.py:103 |
| `FNO_OPERATOR_SESSION_ID` | py | unclear: cli/src/fno/inbox/operator_turns.py:102 |
| `FNO_OPERATOR_TRANSCRIPT` | py | unclear: cli/src/fno/inbox/operator_turns.py:105 |
| `FNO_ORPHANS_SKIP_PROBE` | py | unclear: cli/src/fno/agents/cli.py:3732 |
| `FNO_PANE` | py+rs | unclear: cli/src/fno/agents/cli.py:2784 |
| `FNO_PANE_STATS_EMIT` | rs | unclear: crates/fno/src/server.rs:10563 |
| `FNO_PI_MODEL` | py+rs | unclear: cli/src/fno/agents/harnesses/pi.py:127 |
| `FNO_PI_PROVIDER` | py+rs | unclear: cli/src/fno/agents/harnesses/pi.py:122 |
| `FNO_PLATFORM` | py | unclear: cli/src/fno/context_observation.py:62 |
| `FNO_PROCESS_ADMISSION` | rs | unclear: crates/fno/src/bootstrap.rs:1721 |
| `FNO_PROCESS_ADMISSION_MAX` | py+rs | unclear: cli/src/fno/agents/mux_spawn.py:1914 |
| `FNO_PR_STATUS_BACKOFF_CAP` | py | unclear: cli/src/fno/pr/_cache.py:85 |
| `FNO_PR_STATUS_CACHE_DIR` | py+rs | unclear: cli/src/fno/pr/_cache.py:92 |
| `FNO_PR_STATUS_TTL` | py | unclear: cli/src/fno/pr/_cache.py:78 |
| `FNO_REAL_GH` | py | unclear: cli/src/fno/pr/_quota.py:142 |
| `FNO_RECLAIM_STATE_ROOT` | rs | unclear: crates/fno-agents/src/plugin_install.rs:22 |
| `FNO_RECLAIM_TEMP_ROOT` | rs | unclear: crates/fno-agents/src/reclaim.rs:80 |
| `FNO_REPO_ROOT` | py | unclear: cli/src/fno/outstanding/cli.py:38 |
| `FNO_REVIEW_INVOCATION_ID` | rs | unclear: crates/fno/src/mux_cli.rs:6090 |
| `FNO_ROLES_ROOT` | py | unclear: cli/src/fno/agents/model_routing.py:1644 |
| `FNO_ROUTE_PROVIDER` | py | unclear: cli/src/fno/agent/cli.py:303 |
| `FNO_ROUTE_SETTINGS_DIR` | rs | unclear: crates/fno-agents/src/claude_adopt.rs:112 |
| `FNO_ROUTE_SLOT_DEBUG` | py | unclear: cli/src/fno/rust_binary.py:224 |
| `FNO_RUNTIME_STATE_PATH` | py | unclear: cli/src/fno/adapters/providers/runtime_state.py:186 |
| `FNO_RUST_FRONT` | py | unclear: cli/src/fno/lint_verb_ratchet.py:674 |
| `FNO_SERVER` | py | Names the target mux server. |
| `FNO_SESSION` | py | Deprecated alias of FNO_SERVER. |
| `FNO_SESSION_HARNESS` | py | unclear: cli/src/fno/claims/session_pid.py:223 |
| `FNO_SESSION_PID` | py | unclear: cli/src/fno/claims/session_pid.py:163 |
| `FNO_SIDECAR_SOCKET` | py | unclear: cli/src/fno/mcp/sidecar.py:97 |
| `FNO_SKIP_MIGRATION` | py | unclear: cli/src/fno/cli.py:402 |
| `FNO_SOURCE` | py | unclear: cli/src/fno/update.py:172 |
| `FNO_SPACES_DIR` | py+rs | unclear: cli/src/fno/paths.py:299 |
| `FNO_SPAWN_GATE` | py+rs | unclear: cli/src/fno/agents/spawn_gate.py:1554 |
| `FNO_SPAWN_TRIGGER` | py | unclear: cli/src/fno/agents/dispatch.py:860 |
| `FNO_STORE_KEEPER_DRIFT_CHECK_SECS` | rs | unclear: crates/fno-agents/src/graph_keeper.rs:654 |
| `FNO_STORE_KEEPER_IDLE_SECS` | rs | unclear: crates/fno-agents/src/graph_keeper.rs:115 |
| `FNO_STYLE_ENFORCE` | py | unclear: cli/src/fno/graph/cli.py:926 |
| `FNO_TEST_HERMETIC` | py+rs | unclear: cli/src/fno/hermetic.py:557 |
| `FNO_TEST_MARKER_HOLD_MS` | rs | unclear: crates/fno/src/proto/startup_guard.rs:97 |
| `FNO_TEST_MODE` | py | unclear: cli/src/fno/setup/doctor.py:229 |
| `FNO_TEST_OWNED_HOLD_MS` | rs | unclear: crates/fno/src/proto/startup_guard.rs:109 |
| `FNO_TEST_OWNER_BIRTH` | rs | unclear: crates/fno-agents/src/test_run.rs:145 |
| `FNO_TEST_OWNER_PID` | rs | unclear: crates/fno-agents/src/test_run.rs:144 |
| `FNO_TEST_TIMEOUT_SECONDS` | py | unclear: cli/src/fno/test_runner.py:31 |
| `FNO_THINK_SPAWN_WAVE0` | py | unclear: cli/src/fno/provenance/spawn_think.py:277 |
| `FNO_THREAD_TURN_REFRESH_MS` | rs | unclear: crates/fno-agents/src/codex_thread.rs:105 |
| `FNO_TOUCH_EMIT` | rs | unclear: crates/fno/src/server.rs:10464 |
| `FNO_TRACKER_BACKEND` | py+rs | unclear: cli/src/fno/outstanding/core.py:457 |
| `FNO_TRACKER_GITHUB_REPO` | py | unclear: cli/src/fno/tracker/__init__.py:51 |
| `FNO_UX_SHOTS` | rs | unclear: crates/fno/src/frame_html.rs:356 |
| `FNO_VERIFY_GIT_BIN` | rs | unclear: crates/fno-agents/src/verify_evidence.rs:906 |
| `FNO_WORKER_ADD_DIRS` | rs | unclear: crates/fno-agents/src/claude_ask.rs:687 |
| `FNO_WORKER_NAME` | py | unclear: cli/src/fno/agents/cli.py:2314 |
| `GEMINI_PROJECT_DIR` | py | unclear: cli/src/fno/agent/state.py:145 |
| `GEMINI_SANDBOX` | rs | unclear: crates/fno-agents/src/gemini_ask.rs:103 |
| `GITHUB_ACTIONS` | py | unclear: cli/src/fno/test_cmd.py:1859 |
| `GITHUB_EVENT_BEFORE` | py | unclear: cli/src/fno/lint_cli.py:622 |
| `HERMES_SESSION_ID` | py | unclear: cli/src/fno/adapters/hermes.py:143 |
| `HOME` | py+rs | The user's home directory. |
| `INVOCATION_ID` | py | unclear: cli/src/fno/agents/context.py:94 |
| `MCP_CHANNEL_INBOUND_POKE` | py | unclear: cli/src/fno/agents/context.py:92 |
| `NO_COLOR` | rs | unclear: crates/fno/src/pty.rs:1976 |
| `OUT_DIR` | rs | unclear: crates/fno-agents/build.rs:51 |
| `PATH` | py+rs | Executable search path. |
| `PI_HOME` | py+rs | unclear: cli/src/fno/agents/harnesses/pi.py:132 |
| `POSTMORTEMS_DIR` | rs | unclear: crates/fno-agents/src/finalize.rs:4044 |
| `POSTMORTEM_CORRECTIONS_LOG` | rs | unclear: crates/fno-agents/src/finalize.rs:2890 |
| `POST_MERGE_NONINTERACTIVE` | py | unclear: cli/src/fno/pr/cli.py:918 |
| `PWD` | py+rs | unclear: cli/src/fno/adapters/providers/cli.py:54 |
| `PYTEST_CURRENT_TEST` | py | unclear: cli/src/fno/cli.py:404 |
| `PYTHONPATH` | rs | unclear: crates/fno-agents/src/finalize.rs:1090 |
| `SHELL` | py+rs | The user's login shell. |
| `SMOKE_CHANGED_RECEIPT` | py | unclear: cli/src/fno/test_cmd.py:1726 |
| `SMOKE_FAILURE_RECORD` | py | unclear: cli/src/fno/test_cmd.py:2066 |
| `SMOKE_REGISTRY_FILE` | py | unclear: cli/src/fno/test_cmd.py:2046 |
| `STARSHIP_CONFIG` | py | unclear: cli/src/fno/setup/starship.py:47 |
| `STATE_FILE` | rs | unclear: crates/fno-agents/src/kill_criteria.rs:57 |
| `TARGET_ABORT_REASON` | py | unclear: cli/src/fno/cost/_register.py:480 |
| `TARGET_CLAIM_TTL` | py | unclear: cli/src/fno/target_cli.py:3239 |
| `TARGET_INPUT` | py | unclear: cli/src/fno/target_cli.py:1369 |
| `TARGET_MISSION_ID` | py | Presence marks the post-merge ritual as an autonomous run. |
| `TARGET_NO_MERGE` | py+rs | unclear: cli/src/fno/agents/harness_map.py:215 |
| `TARGET_PLAN_PATH` | py | unclear: cli/src/fno/target_cli.py:1369 |
| `TARGET_SESSION_ID` | py | unclear: cli/src/fno/carveout/core.py:178 |
| `TARGET_SIZE` | py | unclear: cli/src/fno/target_cli.py:1734 |
| `TARGET_SUMMARY_PATH` | py | unclear: cli/src/fno/cost/_register.py:446 |
| `TARGET_UNATTENDED` | py | unclear: cli/src/fno/target/orient.py:255 |
| `TASK_DO_TTL_HOURS` | rs | unclear: crates/fno-agents/src/graph_store.rs:1046 |
| `TASK_LOCK_TTL_HOURS` | py+rs | unclear: cli/src/fno/graph/_constants.py:331 |
| `TERM` | rs | Terminal type; a Rust front terminal capability check reads it. |
| `TMPDIR` | py | unclear: cli/src/fno/events/__init__.py:1772 |
| `USER` | py | unclear: cli/src/fno/adapters/providers/managed.py:185 |
| `USERNAME` | py | unclear: cli/src/fno/adapters/providers/managed.py:185 |
| `USERPROFILE` | rs | unclear: crates/fno-agents/src/publish_review.rs:195 |
| `WORKTREE_STATUS_REGISTRY` | py | unclear: cli/src/fno/agents/registry.py:2525 |
| `XDG_CACHE_HOME` | rs | unclear: crates/fno/src/bootstrap.rs:1395 |
| `XDG_RUNTIME_DIR` | py | unclear: cli/src/fno/mcp/sidecar.py:100 |
| `XDG_STATE_HOME` | py | unclear: cli/src/fno/mcp/client.py:118 |
| `ZDOTDIR` | rs | unclear: crates/fno/src/pty.rs:1770 |

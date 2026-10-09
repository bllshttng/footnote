# Hook Budget Audit

Measured 2026-09-27 from the source registration files and retained event journals. This audit covers Claude, Codex, OpenCode, Pi, AGY, and DeepSeek Harness.

## Command Counts

The command counts include every command entry in each harness event registration. The after column is the source-tree count after the Bash guard consolidation in this change.

| Harness | Registered commands or callbacks | Event coverage |
|---|---:|---|
| Claude | 48 before, 43 after | 16 events |
| Codex | 36 before, 31 after | 9 events |
| OpenCode | 8 logical callbacks | `cli/src/fno/setup/assets/opencode/footnote.js`: `session.created` (report + SessionStart hooks), idle (`session.idle` in 1.x, `session.status` in 2.x), `tool.execute.before` (PreToolUse, deny by throw), `tool.execute.after` (PostToolUse), `chat.message` (UserPromptSubmit), `experimental.chat.system.transform` (drains the injection queue), `experimental.session.compacting` (PreCompact), `shell.env` (identity stamp, 1.x only) |
| Pi | 4 in-process callbacks | `cli/src/fno/setup/assets/pi/footnote.ts`: `session_shutdown`, `resources_discover`, `before_agent_start`, `agent_settled` |
| AGY | 2 command hooks | `crates/fno-agents/src/agy_hooks.rs` registers `Stop` and `PreInvocation` adapters. |
| DeepSeek Harness | 0 in-repository hook registrations | none found |

The Bash registration count drops by five commands per event. The dispatcher keeps all three Rust predicates in one `fno-agents` process and calls the same three Python guards in their existing order. This removes two `fno-agents` binary launches made by the former shell shims. Other same-event guard groups remain separate. A generic shell dispatcher can start each existing script as a child, so it lowers registration counts without cutting process starts.

Other same-event groups retain their own shell or Python entry points. A wrapper that launches each handler adds a process and does not satisfy the spawn-reduction condition. Porting those handlers into one in-process implementation needs separate behavior-preservation evidence and stays outside this measured Bash chain.

| Harness | Event | Before | After | Notes |
|---|---:|---:|---:|---|
| Claude | PreToolUse | 15 | 10 | Six Bash guards became one dispatcher command; all other matcher groups remain. |
| Claude | Notification | 1 | 1 | |
| Claude | UserPromptSubmit | 6 | 6 | |
| Claude | Stop | 4 | 4 | |
| Claude | PostModelSwitch | 1 | 1 | |
| Claude | StopFailure | 1 | 1 | |
| Claude | PostToolUse | 8 | 8 | |
| Claude | SubagentStart | 1 | 1 | |
| Claude | SubagentStop | 2 | 2 | |
| Claude | PreCompact | 2 | 2 | |
| Claude | WorktreeCreate | 1 | 1 | |
| Claude | WorktreeRemove | 1 | 1 | |
| Claude | CwdChanged | 1 | 1 | |
| Claude | FileChanged | 1 | 1 | |
| Claude | SessionEnd | 1 | 1 | |
| Claude | SessionStart | 2 | 2 | |
| Claude | **Total** | **48** | **43** | |
| Codex | PreToolUse | 11 | 7 | Six Bash guards became one dispatcher command; the session-state producer joined as a no-matcher group. |
| Codex | UserPromptSubmit | 6 | 6 | |
| Codex | Stop | 5 | 5 | |
| Codex | PostToolUse | 4 | 4 | |
| Codex | SubagentStart | 1 | 1 | |
| Codex | SubagentStop | 1 | 1 | |
| Codex | PreCompact | 2 | 2 | |
| Codex | PostCompact | 2 | 2 | |
| Codex | SessionStart | 4 | 4 | |
| Codex | **Total** | **36** | **32** | |

The totals are parsed from `hooks/hooks.json` and `hooks/codex-hooks.json`, counting each `hooks[].command`. Claude PreToolUse has 10 commands after the change. Three match `Edit|Write|Bash`. Two match `Edit|Write`. One each matches `Write`, `Edit|Write|NotebookEdit|Bash`, `Bash`, no matcher, and `Skill`.

## Refusal And Injection Actions

The table lists every distinct configured command variant. A command used by both Claude and Codex is listed once, with both registrations named.

| Command variant | Registered event(s) | Action |
|---|---|---|
| `worktree-write-protect.sh` | Claude and Codex PreToolUse (`Edit|Write`) | Refuses edits outside the session's permitted worktree. |
| `write-gate.sh` | Claude and Codex PreToolUse (`Edit|Write|Bash`) | One gate for the three write surfaces: refuses direct writes to the graph store, protected Claude configuration edits, and writes to generated copies (naming their source). Policy in `crates/fno-agents/src/hook/write_gate.rs`; the shell file only resolves the binary. |
| `join-partition-write-guard.sh` | Claude PreToolUse (`Edit|Write`) | Refuses writes to a partition owned by another join participant. |
| `plan-location-guard.sh` | Claude and Codex PreToolUse (`Write`) / (`Edit|Write`) | Refuses plan writes outside the configured plans directory. |
| `lead-delegation-guard.sh` | Claude PreToolUse (`Edit|Write|NotebookEdit|Bash`) | Refuses a titled lead's direct implementation writes. |
| `pretooluse-bash-dispatch.sh` | Claude and Codex PreToolUse (`Bash` / `^Bash$`) | Runs Git protection, deployed-binary copy protection, background-process protection, pipe-result protection, recursive-grep protection, and raw-test protection in their previous order. |
| `session-state.sh claude PreToolUse` | Claude PreToolUse (no matcher) | Marks the session as working. |
| `session-state.sh claude UserPromptSubmit` | Claude UserPromptSubmit | Marks the session as working. |
| `session-state.sh claude Notification` | Claude Notification (`permission_prompt|agent_needs_input`) | Marks the session as waiting for input. |
| `session-state.sh claude Stop` | Claude Stop | Marks the session as done. |
| `session-state.sh claude PostModelSwitch` | Claude PostModelSwitch | Records the model change in the session activity view. |
| `session-state.sh codex UserPromptSubmit` | Codex UserPromptSubmit | Marks the session as working. |
| `session-state.sh codex PreToolUse` | Codex PreToolUse (no matcher) | Marks the session as working; the only signal a goal-mode continuation fires. |
| `session-state.sh codex Stop` | Codex Stop | Marks the session as done and reads the rollout's observed model, effort and sandbox posture. |
| `hooks/inside-leg-report.sh <state>` (stub) | No registration | Tombstone kept one release for out-of-tree callers; maps the state word onto the shim and execs it. |
| `review-hold.sh acquire` | Claude PreToolUse (`Skill`) | Records that the review skill acquired the review hold. |
| `born-with-why-offer-inject.sh` | Claude and Codex UserPromptSubmit | Offers the born-with-why intake prompt when its state says one is due. |
| `inject-mail-notify.sh` | Claude and Codex UserPromptSubmit | Injects unread mail notifications. |
| `inject-announce.sh prompt` | Claude and Codex UserPromptSubmit | Injects queued announcements at the prompt boundary. |
| `inject-announce.sh compact` | Codex PostCompact | Re-injects queued announcements after compaction. |
| `law-stage-inject.sh` | Claude UserPromptSubmit, PostToolUse (`Skill`, `Edit|Write`); Codex UserPromptSubmit, PreToolUse (`Edit|Write`) | Injects the live ruling relevant to the current operation. |
| `prompt-outstanding.sh` | Claude and Codex UserPromptSubmit | Surfaces unresolved operator turns and questions. |
| `target-stop-hook.sh` | Claude and Codex Stop | Runs the target completion gate and holds the session when the mission is incomplete. |
| `context-nudge.sh` | Claude and Codex Stop | Injects the context-budget nudge. |
| `operator-capture-nudge.sh` | Claude Stop; Codex SessionStart | Prompts disposition of captured operator turns. |
| `target-stopfailure.sh` | Claude StopFailure | Records/retries the target stop failure path. |
| `format-on-edit.sh` | Claude and Codex PostToolUse (`Edit|Write`) | Formats the edited file. |
| `edit-integrity.sh` | Claude and Codex PostToolUse (`Edit|Write`) | Checks the edit against the saved pre-edit contents. |
| `spend-drift-monitor.js` | Claude and Codex PostToolUse | Reports spend drift. |
| `claim-heartbeat.sh` | Claude and Codex PostToolUse | Renews the active node claim. |
| `code-review-attest.sh` | Claude PostToolUse (`ReportFindings`), Claude and Codex SubagentStop, Codex Stop | Records the review attestation or a subagent's review result. |
| `capture-plan-mode.sh` | Claude PostToolUse (`ExitPlanMode`) | Saves the approved native plan-mode artifact. |
| `target-subagent-guard.sh` | Claude and Codex SubagentStart and SubagentStop | Checks the target's subagent lifecycle contract. |
| `codex-review-findings.sh` | Codex Stop | Surfaces unaddressed Codex review findings. |
| `save-session.py` | Claude and Codex PreCompact; Claude SessionEnd | Saves the session summary and context data. |
| `precompact-canon-doc.sh` | Claude PreCompact; Codex PreCompact with `FNO_PLATFORM=codex` | Writes the canonical context handoff before compaction. |
| `worktree-setup.sh` | Claude WorktreeCreate | Creates and prepares a managed worktree. |
| `worktree-remove.sh` | Claude WorktreeRemove | Removes a managed worktree using its lifecycle contract. |
| `worktree-env-reload.sh` | Claude CwdChanged and FileChanged (environment-file matcher) | Reloads environment state after directory or environment-file changes. |
| `session-start-using-fno.sh` | Claude SessionStart | Injects Footnote command usage guidance. |
| `context-run.sh claude-session-start` | Claude SessionStart | Starts the Claude session context collection and injection. |
| `context-run.sh codex-session-start` | Codex SessionStart with `FNO_PLATFORM=codex` | Starts the Codex session context collection and injection. |
| `context-run.sh codex-post-compact` | Codex PostCompact with `FNO_PLATFORM=codex` | Rebuilds Codex context after compaction. |
| `context-run.sh codex-whoami` | Codex SessionStart (`startup`) | Injects the Codex session identity at startup. |
| `codex-app-server-nudge-session-start.sh` | Codex SessionStart | Nudges Codex app-server startup when its session is not ready. |

The six commands under the Bash matcher used to launch three shell shims and three Python scripts. Each shell shim started `fno-agents` again. The shared dispatcher removes five registration commands per harness. It runs the three Rust decisions in its own process and invokes the existing Python guards as children. This preserves their source code, event rows, and order.

## Native Extension Surfaces

These surfaces are not shell-command arrays, so their callbacks are listed separately from the Claude and Codex counts.

| Harness | Registered callbacks | Action and process boundary | Journal rate |
|---|---|---|---|
| OpenCode | 1.x `session.created`, `session.idle`, `tool.execute.before`, `tool.execute.after`, `chat.message`, `experimental.chat.system.transform`, `experimental.session.compacting`, `shell.env`; 2.x `session.created`, `session.status` when idle (no `shell.env`) | The in-process bridge runs footnote's hooks.json through the plugin events: guards deny by throw, injections queue into the system prompt, compaction context rides the compacting hook, and `shell.env` stamps the session identity. It also records session creation and runs the shared loop-check gate at idle, re-driving the same session on a non-terminal decision. The adapter shells bounded `fno-agents` calls. | Unknown: no per-callback invocation marker. |
| Pi | `session_shutdown`, `resources_discover`, `before_agent_start`, `agent_settled` | The in-process extension clears per-session state, offers skills, reports the session and injects announcements before a turn, and runs the shared loop-check gate after the agent settles. The adapter uses bounded `fno-agents` child calls. | Unknown: no per-callback invocation marker. |
| AGY | `Stop`, `PreInvocation` | `agy-target-stop-hook.sh` runs the shared completion gate. `agy-team-inject.sh` injects the team prompt only on invocation 0. Both are command hooks installed in AGY's `hooks.json`. | Unknown for callback totals; stop sub-events are journaled, but there is no complete invocation denominator. |
| DeepSeek Harness | No hook registration found | `crates/fno-agents/src/acp_stdio.rs` identifies `deepseek-harness-acp`; it does not register a hook callback. | Not applicable. |

The DeepSeek search was `RIPGREP_CONFIG_PATH= rg -uu -n -g '!.git/**' -g '!**/target/**' -g '!graphify-out/**' 'deepseek-harness|deepseek_harness|deepseekHarness' .`. It found only `crates/fno-agents/src/acp_stdio.rs`, one ACP fixture, and one executor comment. This is evidence for no in-repository hook registration. It does not prove an external DeepSeek Harness version cannot expose one.

## Recorded Guard Decisions

The decision rate is `block / (allow + block)` among rows that carry `type=guard_decision`. The raw project-space logs cover 2026-09-20 20:53:55Z through 2026-09-23 06:14:55Z. The current worktree log covers 2026-09-27 16:47:33Z through 17:42:37Z. These are partial samples, not lifetime rates.

| Guard | Observed allow | Observed block | Recorded block rate | Coverage |
|---|---:|---:|---:|---|
| Git protection | 131 | 0 | 0% | One shared-project event plus 130 current-session events; partial. |
| Background-process guard | 131 | 0 | 0% | One shared-project event plus 130 current-session events; partial. |
| Recursive-grep guard | 131 | 0 | 0% | One shared-project event plus 130 current-session events; partial. |
| Test-run guard | 31,604 | 86 | 0.271% | Retained project files only; partial. |
| Pipe guard | 0 | 0 | Unknown | No matching rows in the scanned project files; incomplete coverage prevents a zero-fire claim. |
| Binary-install guard | 0 | 0 | Unknown | No matching rows in the scanned project files; incomplete coverage prevents a zero-fire claim. |
| Graph-write protection | 47,542 | 1 | 0.0021% | Retained project files only; partial. |
| Lead-delegation guard | 355 | 13 | 3.533% | Retained project files only; partial. |
| Join-partition, generated-write, and worktree-write guards | 98 each | 0 each | 0% observed | Retained project files only; partial. |
| Plan-location guard | 22 | 0 | 0% observed | Retained project files only; partial. |

Other configured commands do not emit a per-invocation marker that can serve as a denominator. Their hit rates are unknown, not zero. `fno doctor event find guard_decision --limit 1 -J` also reports `coverage: partial` and `complete_count: null`, so these rows do not justify deleting an installed guard.

## Proven Orphan

`hooks/law-session-start.sh` is a no-op tombstone with no registration or dynamic caller in the complete caller sweep `RIPGREP_CONFIG_PATH= rg -uu -n -g '!.git/**' -g '!**/target/**' -g '!graphify-out/**' 'law-session-start\.sh|hooks/hooks\.json' .`. It remains for the compatibility release named in its own lifecycle comment. Its removal is not counted as a safe dispatch saving. Other hooks with partial or absent invocation telemetry stay installed.

## Timing

CI `hook_budget_bash_pretooluse_dispatch` completed on PR head `b03303a7dfa6`: p90 420.8 ms, maximum 436.2 ms, `exec_count=11`, within the 1000 ms budget. The prior measured head reported 14 execs. The pinned event-path fast path removes three path-discovery execs. The final local macOS run on 2026-09-29 measured p90 334.6 ms and maximum 434.9 ms across 100 samples. It reported `partial_exec_count=4` and was advisory. The native event journal cannot measure harness dispatch overhead.


## Hook Jobs By Harness

The capability table's `hooks.<job>` rows, one cell per declared state (`supported` with its via paths, `impossible` with the measured reason, or `missing` where fno has no registration). The table load refuses a `missing` job on a wired row (a row whose loop runs or that supports any other job), so a gap here is a build break, never an audit-only fact.

| Harness | lead_guard | lead_reinject | session_state |
|---|---|---|---|
| claude | supported | supported | supported |
| codex | supported | supported | supported |
| opencode | supported | supported | supported |
| pi | supported | supported | supported |
| agy | supported | impossible (no compaction event; role lands at session start) | supported |
| footnote | supported | impossible (in-process compaction fires no hook event) | supported (native: the harness writes fno's own records) |
| gemini | missing | missing | missing |
| cursor-agent | missing | missing | missing |
| grok | missing | missing | missing |
| zcode | missing | missing | missing |

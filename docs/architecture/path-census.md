# Path census

This is the living manifest for the path-consolidation epic.
Every consolidation PR closes the rows it retires or deletes by recording its PR number in the final column.
A PR that adds a second path to a censused operation must add a row with a justification before adding the path.
`OPEN` means the row remains work for a later child; `TBD` is replaced with this PR number when this PR opens.
Rows marked `OPEN` below are intentionally not deleted when the repository still has production callers; their call sites are recorded so a later migration can close them safely.

## Census 1: post-merge ritual

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | Detector: `fno backlog reconcile` backstop | `cli/src/fno/graph/cli.py:6706-6722` via `hooks/reconcile-session-start.sh` | RETIRE dispatch leg | OPEN |
| 2 | Detector: pr-watch LaunchAgent daemon, 600s poll | `cli/src/fno/pr_watch/_dispatch.py:696-746`, `_install.py` | KEEP, sole detector | — |
| 3 | Warm inject into live origin session | `cli/src/fno/post_merge_route.py:72-150`, `_reconcile.py:1411-1445` | KEEP, canonical delivery | — |
| 4 | Direct-finalize rung | `_reconcile.py:1447-1477` | KEEP | — |
| 5 | Cold `claude --bg` Sonnet session | `_reconcile.py:1560-1606` | DELETE | OPEN |
| 6 | Cold headless `claude --print` `fire_skill` | `cli/src/fno/pr_watch/_dispatch.py:212-231,720-735` | RETIRE through canonical spawn | OPEN |
| 7 | LLM wrapper around `merged.md` | `skills/pr/references/merged.md` | RETIRE wrapper | OPEN |

## Census 2: backlog grooming

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno backlog groom` pipeline | `cli/src/fno/backlog/groom.py:239-365` | KEEP, canonical | — |
| 2 | `scripts/nightly-groom.sh` | whole file, execs the verb | DELETE | TBD |
| 3 | `fno backlog triage health` | `graph/cli.py` | KEEP, separate metrics verb | — |

## Census 3: post-merge addendum

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `scripts/post-merge/watch.sh` launchd fire point | `watch.sh:71` | DELETE | TBD |

## Census 4: worker/agent spawn

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno agents spawn` / `dispatch_spawn` | `agents/dispatch.py:2011`, `agents/cli.py:486` | KEEP, canonical | — |
| 2 | pr-watch `fire_skill` hand-assembled `claude --print` | `pr_watch/_dispatch.py:143,216` | RETIRE through canonical spawn | OPEN |
| 3 | `scripts/post-merge/watch.sh` hand-assembled `claude --print` | `watch.sh:71` | DELETE | #573 |
| 4 | Rust `ShelloutDispatcher` -> `driver-claude-code.sh` | `crates/fno-agents/src/loop_megawalk.rs:1208` | RETIRE, migration needed after reachability trace | OPEN |
| 5 | Python megawalk walker + `ClaudeCodeDriver` | `megawalk_drivers/claude_code.py:53` | DELETE | #573 |
| 6 | Claude/Codex adapter worker spawns | `adapters/{claude_code,codex}.py` | RETIRE after live callers migrate (see below) | OPEN |
| 7 | One-shot `claude -p` LLM-as-a-function | `inbox/triage.py:304` and three sites | OUT OF SCOPE | — |
| 8 | Gemini provider adapter paths | `agents/dispatch.py` Gemini create/follow-up/reconcile paths, `agents/providers/gemini.py` | DELETE; Rust keeps its native provider path and harness-map refusal | TBD |
| 9 | Shell-form `claude -p` in the memory pass | `scripts/memory/post-merge-pass.sh:12` | RETIRE through canonical spawn (surfaced by the lint's shell-form scan) | OPEN |

The Claude/Codex adapter row was closed after its live review caller migrated to canonical one-shot dispatch.
The Gemini row is closed by removing the Python adapter and its dispatch-only tests while retaining readable legacy registry identities, pane support, and the harness-map refusal pointing at agy.

Leg 6 reachability trace (2026-07-23): `loop_target.rs:420-427` dispatches `--driver megawalk` to `loop_megawalk::run`; `loop_megawalk.rs:1153-1155` constructs `ShelloutDispatcher`; `loop_dispatch.rs:250-272` implements the live dispatcher.
The dispatcher is reachable, so this PR does not partially delete it or `scripts/lib/driver-claude-code.sh`; the row remains `RETIRE (migration needed)` for the later driver cutover.

## Census 5: session liveness / observation

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno agents truth` | `agents/session_truth.py:151` | KEEP, alive/working truth | — |
| 2 | `truth_status` + manifest liveness | `agents/truth_status.py:182`, `target/orient.py:181` | KEEP, work ownership truth | — |
| 3 | `discover_live_sessions` liveness verdict | `agents/discover.py:1585` | RETIRE verdict; keep enumeration | OPEN |
| 4 | `peek` | `agents/peek.py:617` | KEEP | — |
| 5 | Claim PID/TTL classify | `claims/staleness.py:51` | KEEP inside ownership family | — |
| 6 | `control.sock` probe | `claude_ask.rs:657`, `recovery.py:962` | RETIRE as truth; keep pre-filter | OPEN |
| 7 | `recovery.classify` on `state.json` | `recovery.py:83` | RETIRE; repoint at family 1 | OPEN |
| 8 | `lsof` | — | PHANTOM, not in source | — |

## Census 6: mail / message delivery

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | Claude control.sock inject | `agents/dispatch.py:4340` | KEEP | — |
| 2 | Owned-PTY worker.sock submit | `roundtrip.submit_via_worker` | KEEP | — |
| 3 | Mux pane send | `agents/dispatch.py:4175`, `mail/cli.py:1125-1145` | KEEP | — |
| 4 | Codex app-server inject | `agents/dispatch.py:4496` | KEEP | — |
| 5 | Wake-and-deliver revival | `agents/dispatch.py:4371`, `mail/cli.py:945` | FIX with incarnation lease | OPEN |
| 6 | Durable bus write | `inbox/store.py:745` | KEEP, fallback | — |

## Census 7: graph reads + backlog mutation

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `read_graph` / `read_graph_strict` | `graph/store.py:543,556` | KEEP, canonical Python seam | — |
| 2 | `load_graph` hash-integrity reader | `graph/load.py:85` | RETIRE divergence | OPEN |
| 3 | `read_graph_nodes` raw scoreboard reader | `scoreboard/fold.py:122` | RETIRE; replace with #1 | OPEN |
| 4 | Rust mux reader | `crates/fno/src/backlog_view.rs:29-41` | KEEP, pin schema version | OPEN |
| 5 | Shell resolver heredoc + grep fallback | `scripts/lib/graph-resolve.sh:61,132` | RETIRE; call strict verb | OPEN |
| 6 | Raw hook/skill grep and JSON readers | `init-target-state.sh:1183`, `autolaunch-on-ready.sh:106,178`, `autocorrect-pack.sh:26` | RETIRE where a verb exists | OPEN |

## Census 8: worktree creation

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno worktree ensure` / `fno target start` | `worktree_cli/cli.py:282`, `target_cli.py:900` | KEEP, canonical autonomous path | — |
| 2 | Raw `git worktree add` + linker | `scripts/setup/setup-worktree.sh` | KEEP, manual path converges | — |
| 3 | Conductor UI recipe | `conductor.json:3`, `worktree-create-hook.sh` | KEEP, converge on linker | — |
| 4 | Claude WorktreeCreate hook | `hooks/worktree-setup.sh` | RETIRE duplicate setup | OPEN |
| 5 | `/speculate` private setup | `skills/speculate/scripts/worktree-setup.sh` | RETIRE duplicate setup | OPEN |
| 6 | Harness EnterWorktree | harness tool | KEEP, enters only | — |

## Census 9: test running

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno test` / `fno test rust` | `cli/src/fno/test_cmd.py:228,178` | KEEP, canonical | — |
| 2 | CI `uv run pytest` inside `smoke.sh` | `.github/workflows/cli-ci.yml:108` -> `scripts/ci/smoke.sh:67` | RETIRE divergence | OPEN |
| 3 | Hand-enumerated shell tests | `smoke.sh:69-93` | RETIRE registry; auto-discover | OPEN |
| 4 | Bare `pytest` | user-invoked | KEEP external tool; warn in worktrees | OPEN |
| 5 | RTK wrappers | RTK config | KEEP bypass guard | — |

## Census 10: work-claim / duplicate-thread prevention

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | Pre-claim launch window | twin worker launched before either claim was visible (observed 2026-07-22) | FIX: claim before observable work + visibility barrier | OPEN |
| 2 | No fixed-on-main check at filing | retro-triage minted a node for a finding already fixed on main in parallel (observed 2026-07-23) | FIX: record and check finding anchor | OPEN |
| 3 | No still-broken probe at dispatch | a worker spent nine review rounds on a mechanism main had deleted | FIX: pre-spawn anchor probe and closure | OPEN |

### Leg 6 reachability evidence (2026-07-23)

The Rust dispatcher is reachable: `crates/fno-agents/src/loop_target.rs:420-427` routes `--driver megawalk` to `crate::loop_megawalk::run`, `loop_megawalk.rs:1153-1155` constructs `ShelloutDispatcher`, and `loop_dispatch.rs:250-272` implements its live `Dispatcher` path.
This leg therefore records `RETIRE (migration needed)` and deletes neither `ShelloutDispatcher` nor `scripts/lib/driver-claude-code.sh`.

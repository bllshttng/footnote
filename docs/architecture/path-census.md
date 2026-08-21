# Path census

This is the living manifest for the path-consolidation epic.
Every consolidation PR closes the rows it retires or deletes by recording its PR number in the final column.
A PR that adds a second path to a censused operation must add a row with a justification before adding the path.
`OPEN` means the row remains work for a later child; `TBD` is replaced with this PR number when this PR opens.
Rows marked `OPEN` below are intentionally not deleted when the repository still has production callers; their call sites are recorded so a later migration can close them safely.

## Census 1: post-merge ritual

The dispatch lives in one leaf module: `cli/src/fno/post_merge_route.py`
(`decide_post_merge_route` + `dispatch_post_merge_ritual` + the receipt). pr-watch
is the sole detector; the cold path runs the mechanical `fno do pr ritual <pr>
--autonomous` verb directly (no bg thread, no `/fno:pr merged` LLM wrapper), and
the warm route injects that SAME verb. The receipt is attribution only, never a
dedup input (the marker + TTL claim remain the idempotency layer).

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | Detector: `fno backlog reconcile` backstop | was `graph/cli.py` dispatch leg (deleted); reconcile now closes nodes + stamps plans + advances only | RETIRED dispatch leg | #587 |
| 2 | Detector: pr-watch LaunchAgent daemon, 600s poll (SOLE detector) | `cli/src/fno/pr_watch/_dispatch.py` `_default_dispatch_ritual` -> `post_merge_route.dispatch_post_merge_ritual`, `_install.py` | KEEP, sole detector | — |
| 3 | Warm inject into live origin session (the verb, not an LLM prompt) | `cli/src/fno/post_merge_route.py` `inject_pr_merged` (WARM_PROMPT = `fno do pr ritual <pr> --autonomous`) | KEEP, canonical delivery | — |
| 4 | Direct-finalize rung | `cli/src/fno/post_merge_route.py` `_finalize_origin_ledger` (cold prelude, falls through to the verb) | KEEP | — |
| 5 | Cold `claude --bg` Sonnet session | was `_reconcile._spawn_post_merge_worker` (deleted) | DELETED; cold path runs the verb directly | #587 |
| 6 | Cold headless `claude --print` `fire_skill` merged branch | `cli/src/fno/pr_watch/_dispatch.py` `fire_skill` (check-only) | RETIRED merged branch; the review `check` fire is retained | #587 |
| 7 | LLM wrapper around the ritual | the `fno do pr ritual` verb owns the mechanical core; pr-watch runs it directly with no whole-ritual model layer | RETIRED wrapper paths | #587 |

The merge-SHA marker, `post-merge-ritual:<sha>` TTL claim, and `reconcile:pr-<n>`
ritual claim remain in place through a seven-day observation window; only after
that window may a trigger-based cleanup retire the marker layer (the TTL claim
stays as the single idempotency floor).


## Census 2: backlog grooming

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno backlog groom` pipeline | `cli/src/fno/backlog/groom.py:239-365` | KEEP, canonical | — |
| 2 | `scripts/nightly-groom.sh` | whole file, execs the verb | DELETE | #573 |
| 3 | `fno backlog triage health` | `graph/cli.py` | KEEP, separate metrics verb | — |

## Census 3: post-merge addendum

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `scripts/post-merge/watch.sh` launchd fire point | `watch.sh:71` | DELETE | #573 |

## Census 4: worker/agent spawn

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno agents spawn` / `dispatch_spawn` | `agents/dispatch.py:2011`, `agents/cli.py:486` | KEEP, canonical | — |
| 2 | pr-watch `fire_skill` hand-assembled `claude --print` | `pr_watch/_dispatch.py:143,216` | RETIRE through canonical spawn | OPEN |
| 3 | `scripts/post-merge/watch.sh` hand-assembled `claude --print` | `watch.sh:71` | DELETE | #573 |
| 4 | Rust `ShelloutDispatcher` -> `driver-claude-code.sh` | `crates/fno-agents/src/loop_dispatch.rs` | KEEP; serves the target driver (megawalk path removed) | closed |
| 5 | Python megawalk walker + `ClaudeCodeDriver` | `megawalk_drivers/claude_code.py:53` | DELETE | #573 |
| 6 | Claude/Codex adapter worker spawns | `adapters/{claude_code,codex}.py` | DELETE; review caller migrated to canonical dispatch | #573 |
| 7 | One-shot `claude -p` LLM-as-a-function | `inbox/triage.py:304` and three sites | OUT OF SCOPE | — |
| 8 | Gemini provider adapter paths | `agents/dispatch.py` Gemini create/follow-up/reconcile paths, `agents/harnesses/gemini.py` | DELETE; Rust keeps its native provider path and harness-map refusal | #573 |
| 9 | Shell-form `claude -p` in the memory pass | `scripts/memory/post-merge-pass.sh:12` | RETIRE through canonical spawn (surfaced by the lint's shell-form scan) | OPEN |

The Claude/Codex adapter row was closed after its live review caller migrated to canonical one-shot dispatch.
The Gemini row is closed by removing the Python adapter and its dispatch-only tests while retaining readable legacy registry identities, pane support, and the harness-map refusal pointing at agy.

Leg 6 reachability trace (2026-07-23): `loop_target.rs:420-427` dispatched `--driver megawalk` to `loop_megawalk::run`; `loop_megawalk.rs:1153-1155` constructed `ShelloutDispatcher`; `loop_dispatch.rs:250-272` implemented the live dispatcher.
The `--driver megawalk` arm and `loop_megawalk.rs` were removed on 2026-08-03; `ShelloutDispatcher` (now in `loop_dispatch.rs`) survives, serving the target driver.

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
| 1 | `read_graph` / `read_graph_strict` | `graph/store.py:555,594` | KEEP, canonical Python seam | — |
| 2 | `load_graph` hash-integrity reader | `graph/load.py:148` | CLOSED; `_entries` routes through `_apply_graph_defaults` | this PR |
| 3 | `read_graph_nodes` raw scoreboard reader | `scoreboard/fold.py:186` | CLOSED; delegates to #1 | this PR |
| 4 | Rust mux reader | `crates/fno/src/backlog_view.rs:60` | CLOSED; vocabulary-coverage test, NOT a schema-version stamp | this PR |
| 5 | Shell resolver heredoc | `scripts/lib/graph-resolve.sh:66,77` | CLOSED; reads via `read_graph_strict`, keeps `resolve_id` | this PR |
| 5b | Shell resolver legacy grep fallback | `scripts/lib/graph-resolve.sh` `_resolve_arg_legacy` | KEEP; the no-`fno` escape hatch, reachable only when the package will not import | — |
| 6a | `init-target-state.sh` grep presence checks | `init-target-state.sh:672,1118` | CLOSED; `fno backlog get --strict`, and the second check deleted as redundant | this PR |
| 6b | `autocorrect-pack.sh` blocked-state read | `autocorrect-pack.sh:183` | CLOSED; read `.entries`, not `.nodes` | this PR |
| 6c | plan_path -> node lookup heredocs | `autolaunch-on-ready.sh:108`, `init-target-state.sh:921` | OPEN, deliberately parked | OPEN |

Row 4 deviates from the original disposition.
A version stamp needs a writer change plus an integer someone must remember to bump, and the only sane failure action for a display surface is to keep rendering, which it already does.
`classify` now names every status explicitly (including the ones it drops), so `_ => None` means "unknown" rather than doubling as the drop bucket, and `cli/tests/unit/test_graph_status_vocabulary.py` compares that set against `VALID_STATUSES` + `STATUS_MIGRATION`.
Same drift caught, no new field on disk.

Row 5 deviates too.
The census said to call `fno backlog get --strict`, but the two resolvers are not ordered by capability: `get` uses `resolve_node`, which resolves a bare hex `ff6f96e0` that `resolve_id` misses and misses a partial `ab-ff6f96` that `resolve_id` resolves.
Swapping trades one tested capability for another and drops the `RESOLVE_FUZZY` title-match path.
What was actually the landmine -- the hand-rolled `json.load` -- is gone.

Row 6c stays open on purpose: two heredocs resolve a node by `plan_path`, no verb does that, and minting one to serve two shell callers is more surface than the duplication costs.
Neither reads a field `_apply_graph_defaults` rewrites, so neither is a drift landmine; they are ordinary duplication for a later sweep.

## Census 8: worktree creation

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno worktree ensure` / `fno do target start` | `worktree_cli/cli.py:282`, `target_cli.py:900` | KEEP, canonical autonomous path | — |
| 2 | Raw `git worktree add` + linker | `scripts/setup/setup-worktree.sh` | KEEP, manual path converges | — |
| 3 | Conductor UI recipe | `conductor.json:3`, `worktree-create-hook.sh` | KEEP, converge on linker | — |
| 4 | Claude WorktreeCreate hook | `hooks/worktree-setup.sh` | RETIRE duplicate setup | OPEN |
| 5 | `/speculate` private setup | `skills/speculate/scripts/worktree-setup.sh` | RETIRE duplicate setup | OPEN |
| 6 | Harness EnterWorktree | harness tool | KEEP, enters only | — |

## Census 9: test running

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | `fno test` / `fno test rust` / `fno test smoke` | `cli/src/fno/test_cmd.py` | KEEP, canonical | — |
| 2 | CI smoke job | `.github/workflows/cli-ci.yml` -> `uv run fno-py test smoke` | DONE (was pytest-in-smoke.sh; one entry now) | — |
| 3 | Shell test registry | `cli/src/fno/test_cmd.py` (`_STRUCTURAL_STEPS` + `discover_shell_harnesses`) | DONE (auto-discover owned trees; smoke.sh retired) | — |
| 4 | Bare `pytest` | user-invoked | KEEP external tool; warn in worktrees | OPEN |
| 5 | RTK wrappers | RTK config | KEEP bypass guard | — |

## Census 10: work-claim / duplicate-thread prevention

| # | Path | Entry | Disposition | Closing PR |
|---|---|---|---|---|
| 1 | Pre-claim launch window | twin worker launched before either claim was visible (observed 2026-07-22) | FIX: claim before observable work + visibility barrier | OPEN |
| 2 | No fixed-on-main check at filing | retro-triage minted a node for a finding already fixed on main in parallel (observed 2026-07-23) | FIX: record and check finding anchor | OPEN |
| 3 | No still-broken probe at dispatch | a worker spent nine review rounds on a mechanism main had deleted | FIX: pre-spawn anchor probe and closure | OPEN |

### Leg 6 reachability evidence (2026-07-23)

The Rust dispatcher was reachable via the megawalk driver (removed 2026-08-03); `ShelloutDispatcher` (now in `loop_dispatch.rs`) is retained for the target driver, as is `scripts/lib/driver-claude-code.sh`.

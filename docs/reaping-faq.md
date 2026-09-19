# Reaping FAQ: why is this row still here?

A reaping sweep judged a session row and kept it. This page names the keep reason and tells you what to do. It covers every program that stops or removes a session or one of its parts. It states the order they run in. It names every key of `fno agents reap --json`.

Run `fno agents reap --dry-run` and find your row handle in the report. The dry run classifies every row, names one reason per row, and writes nothing. It stops no process, prunes no tree, and writes no receipt.

Measured on this machine on 2026-09-10 with the dry run: 1 `would retire` line, 49 `kept` lines, 1 `held` line. The largest bucket was `open work` with 22 rows. Eight rows sat behind a permanent exemption: 2 operator rows, 4 crowned rows, 2 adopted rows. Counts like these move within the hour, so run your own dry run and date the result.

## Is this page for you?

A reaping sweep kept a session row and you want to know why. You want a row gone and reap refuses. You asked which condition acts first and got told no doc answers. This page owns the keep reasons and every program that stops or removes a session or one of its parts. It owns the order they run in. It owns every key of the reap JSON report. Misreading it makes you force a delete the machine will re-judge on the next sweep, or kill a worker whose row was telling the truth.

Not for: the worktree removal contract (which trees prune on merge and which never do). That is answered at [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) and owned by [../.claude/rules/worktrees.md](../.claude/rules/worktrees.md).

## Read the answer

Every report line has three parts: the verb, the row handle, and the reason in parentheses.

```
kept fb6399d5 (not a spawn row: origin adopted)
```

The verb names the verdict. `kept` means the sweep declined this pass. `held` means the rehearsal declined to promise an action. `would retire` is the dry-run remove verdict, and `retired` is the real one.

The first report line carries the counts. The last lines carry the dry-run marker and the mux sweep line. A retire line names its basis, for example `every named node done`, with the witnesses that agreed.

Every daemon tick with held rows also writes one journal row you can read without the verb. The row carries one entry per held handle, with its reason, detail, age, and escalation flag. Read it with:

```
rg '"type":"retire_holds"' ~/.fno/agents/events.jsonl
```

A tick with zero holds writes no `retire_holds` row, so silence is the zero-reading.

## Why is my finished worker still on the roster

Three keeps used to hold finished rows with no way out. Each now reads a live reason, and each keeps refusing a specific wrong answer.

- An open-PR hold asks GitHub once the row is quiet past the grace. A closed or merged PR releases the row. An open PR keeps it, and so does an unread answer, under `pr state contradicts`. A fresh row is never read and never released by this arm.
- An adopted row keeps while its harness record says the session exists. A recorded pid that answers ESRCH releases it to the ordinary gates. So does a claude row missing from a known `claude agents` read. An unknown or partial roster read keeps the row: a failed instrument is never absence. The registry keep is not the whole story. An adopted, uncrowned row does not shield its listed session from the roster sweep.
- When its inside-leg report reads done and fno never stopped it, a no-node row releases once its transcript goes quiet. A row still working, blocked, or stopped by fno keeps. The keep now carries a clock, so `fno agents reap --release` reaches it.

Quiet is only ever the second conjunct. Every release rests on a positive marker: GitHub answering, a roster read answering, a dead pid, the worker's own done report. Silence alone releases nothing.

## Ten programs stop or remove a session

Ten programs stop, retire, or remove a session or one of its parts. A reader who watches one and concludes the others are broken has mixed them up. This exact confusion cost a real session: the arms readout showed `acted=0 skip=held` while the manual verb retired 3 rows in the same minute. Each row cites its entry point as file plus symbol, because a line cite rots and a symbol cite survives a move.

| # | Program | Entry point | Trigger and cadence | Removes | Keeps | What it reads about an open PR |
|---|---|---|---|---|---|---|
| 1 | Registry sweep | `gc.rs` `gc_sweep`, policy `gc.rs` `gc_decide`; daemon arm `gc.rs` `maybe_retirement_sweep`; manual verb `client.rs` `run_reap` | daemon `agents.retire_interval_s`, default 300 s (`agents_config.rs` `DEFAULT_RETIRE_INTERVAL_SECS`), one pass in flight; grace `agents.retire_grace_s`, default 900 s (`agents_config.rs` `DEFAULT_RETIRE_GRACE_SECS`) | the session process, the native surface, the registry row, and a clean-and-merged worktree | the receipt with the resume command, the transcript, the branch, a dirty or unmerged tree | an open PR holds the row once it is quiet past the grace; `gc_sweep.rs` `open_pr_verdict` reads GitHub once per pass |
| 2 | Roster sweep | `roster_reap.rs` `roster_reap`; scheduled in the retire arm (`gc.rs` `maybe_retirement_sweep`); manual `fno-agents roster-reap` (`client.rs` `run_roster_reap`) | the retire cadence at scope `agents.reap.roster_scope`, default `Provenanced` (`agents_config.rs` `DEFAULT_ROSTER_SCOPE`); the manual verb is a dry run until `--apply` | claude sessions no fno row owns: the native session, with a receipt | open work at the default scope, even on a terminal harness state | reads only provenance: a `sessions[]` row fno wrote or a staged reap receipt makes the session owned; PR state is not read |
| 3 | Mux tab prune | `gc.rs` `mux_tab_sweep`; daemon default flags in the retire arm (`gc.rs` `maybe_retirement_sweep`); the manual verb adds `--include-used-shells` (`client.rs` `run_reap`) | every registry pass; the manual verb runs it too, `--no-mux` skips it there | the mux tab of a worker the mux server reads dead | a live pane, a used shell unless `--include-used-shells` | not read |
| 4 | Merge reaper | `merge_reap.rs` `consume_merge_cleanup_requests` | a recorded merge cleanup request; 60 s floor (`merge_reap.rs` `MERGE_REAP_INTERVAL_SECS`), expiry 86400 s (`merge_reap.rs` `MERGE_REAP_EXPIRY_SECS`) | rows of the merged tree through the shared stage and commit, then the tree itself, with force | crowned and operator rows, the branch, a tree whose HEAD is not on `origin/main` | reads each request node's `status` and `merge_status`; additional PRs are not read |
| 5 | Worktree cleanup sweep | `daemon/worktree_sweep.rs` `worktree_sweep`; manual `fno agents workspace worktree cleanup --merged` (`scripts/lib/worktree-lifecycle.sh`) | a 21600 s interval (`daemon/worktree_sweep.rs` `WORKTREE_SWEEP_INTERVAL_SECS`), and it applies only while a merge cleanup request waits (`merge_reap.rs` `merge_cleanup_requested`); the manual verb is a dry run until `--apply` | the tree through the archive script, then dead job records | the branch, and every unpushed, unmerged, live-claim, or unborn-branch tree | reads git ancestry, not the PR state: merged means reachable from `origin/main` |
| 6 | Terminal-stop marker sweep | `daemon.rs` `terminal_stop_sweep`; markers `terminal_stop.rs` `write_marker` and `read_markers` | markers `finalize` writes on a terminal loop decision; read on every daemon tick | the session process through `claude stop`, with a 15 s bound, and the marker | the row, the tree, the node, and the claim; the roster state goes terminal, and the registry sweep retires the row on a later pass | not read; the loop's finish line is before merge by design |
| 7 | `fno agents rm` | `daemon.rs` `handle_rm` and `handle_rm_with` | a person, the post-merge ritual, or the watchdog sandbox lane | the row, the native session, the pane, and a reapable tree | the transcript | not read; rm reads no node state |
| 8 | Post-merge ritual row removal | `cli/src/fno/pr/_ritual.py` (the archive leg, and the `reap-rows` leg when `self_reap` is on) | `/fno:pr merged` after `gh` reads `MERGED` | rows through `fno agents rm` | the transcript and the branch | merged by construction; additional PRs are not read |
| 9 | Watchdog and recovery lanes | lane sets `watchdog.py` `LANES`, apply `watchdog.py` `apply_verdict`, gate `watchdog.py` `_gate_reason`; keeper `keeper_lane.py` `reap_keepers`; stop-then-respawn `recovery.py` `recovery_sweep`, `_redispatch`, `_respawn_bg_resume`, `_revive_bg_thread` | the pr_watch tick when `recovery.watchdog.enabled`; keeper and recovery legs have their own gates | wake and silence remove nothing (they resume); the sandbox lane force-rms a codex row; a keeper reap group-kills; a recovery leg stops a stale worker and respawns one in the same tree | a live claim or owner keeps the row; a failed spawn after a stop can leave the node driverless | checks only that the node is not done; it does not read the PR |
| 10 | Orphan process sweeps | fno-py orphans in the retire arm (`gc.rs` `unowned_sweeps`); test binaries `orphan_reap.rs` `maybe_sweep` on a 300 s cadence; keeper registry sweep at daemon start (`daemon.rs` `keeper_registry_sweep`); manual `fno agents orphans --reap` | daemon cadences or a person | the process: ppid 1, age past its floor, pid not in the registry | a registry pid is excluded, so a tracked worker is safe; dead keepers' sockets are unlinked | not read |

The manual verb and the registry arm run the same sweep body (`client.rs` `run_reap`, `gc.rs` `gc_sweep`). The registry arm runs one sweep at a time behind a one-in-flight gate (`gc.rs` `maybe_retirement_sweep`). A slow sweep holds the next request, so the effective cadence is not the interval. Run `fno-agents status` to read the arms table.

The merge-request arm is a different program (`merge_reap.rs` `consume_merge_cleanup_requests`). It loops only over pending merge cleanup requests. With no pending request it does nothing. Its skip reasons are `no_requests`, `all_in_grace`, and `held`, and its detail line carries the held count beside the request count. So its `acted=0 skip=held` line says nothing about the registry sweep.

A fourth tool answers to a related name. `fno-agents roster-reap` removes claude rows that fno never registered. The daemon's retire arm schedules it after the registry sweep at the configured `agents.reap.roster_scope`, with `dry_run` false. The manual verb stays a dry run by default, and `--apply` acts (`client.rs` `run_roster_reap`). The scheduled pass retires only on an ownership marker. The marker is a `sessions[]` row fno wrote, or a reap receipt an earlier retirement staged. Weak provenance, a name pattern or a transcript mention, keeps.

## Neighbors that remove no session

These five move or remove state around sessions. None stops or removes a session, so do not look for them in the table above.

- Claim reclaim: lazy, on a competing `acquire`. The old lockfile moves to `.expired/` (`claims.rs` `acquire`, `claims.rs` `classify`). No daemon arm reclaims claims.
- The nudge ladder: keeps the row and sends input instead (`pr_nudge.rs` `run_ladder`). It fires on the daemon arm only. The manual dry run prints its plan as `would nudge {id} ({action})` and takes no effect.
- The state-file sweep: removes expired claims, stale plan locks, agent locks, the pr-status cache, and claim tmp files (`gc.rs` `state_file_sweep`). No row is touched.
- The liveness sweep: bands the machine and writes status. It removes nothing (`daemon.rs` `liveness_sweep`).
- The daily reclaim janitor, with its `cargo_build_dirs` lane: removes disk artifacts, never a session (`reclaim.rs` `maybe_run_daily`, `reclaim.rs` `cargo_build_dirs_lane`).

## In what order

**Across programs, no arbiter exists.** Each daemon arm runs off-loop behind its own one-in-flight gate. The tick's issue order is the retire arm, then the worktree task, then the orphan test-binary sweep, then liveness, then terminal-stop (`daemon.rs` select arm). That issue order is not a completion order. The first program to act wins and the others find the row gone. The merge reaper and the retire arm share one stage and commit. The merge reaper calls them with no release, so it is the stricter of the two.

**Inside the retire arm**, in this order: state-file sweep, registry sweep, nudge ladder, unowned sweeps, roster sweep, mux tab prune (`gc.rs` `maybe_retirement_sweep`). The roster sweep runs after the registry sweep on purpose. A row the registry sweep retires this pass is already gone from the registry the roster sweep loads. A session the roster sweep removes becomes a corpse for the next registry pass.

**Inside the registry sweep, per row**, the first gate that answers decides. Fourteen steps, derived from `gc_sweep.rs` `run_with_release` and `gc.rs` `gc_decide`:

1. operator row: `kept {id} (operator row)`
2. crowned row: `kept {id} (crowned)`
3. not spawn unless a proven corpse: `kept {id} (not a spawn row: {why})`
4. graph unreadable: `kept {id} (graph unreadable: never a retirement on a failed read)`
5. open do row on an all-done session: `kept {id} (open do row on done node: {node})`. The settle pass and a `--release` ruling work through this gate
6. policy `gc_decide`: a confirm hold answers as `kept {id} (sources disagree: {a} vs {b})` or `kept {id} (pr state contradicts: {node} {detail})`
7. policy `gc_decide`: no provenance: `kept {id} (no provenance: ...)`
8. policy `gc_decide`, open node: planning lane, then open-PR keep `kept {id} (open pr: {node} {detail})`, then the four releases, then the open-work window
9. the grace gate: an unresolved transcript keeps, a fresh transcript keeps unless terminal or the pid is gone
10. live descendant: `kept {id} (live descendant: {child})`, skipped for a terminal row
11. apply freshness re-check: `kept {id} (active: ...)` or `kept {id} (probe unread: ...)`
12. the stop gate and receipt stage: `held {id} (needs live stop: {reason})`, `kept {id} (stop refused: {reason})`, `kept {id} (no resumable receipt: {reason})`
13. the dry-run unverified gate: `held {id} (dry-run did not evaluate: {gate}; apply may still refuse)`
14. the tree verdict, then the commit: `kept tree {id} (...)` lines, then the retire lines

## A dry run and a real run answer different questions

A dry run classifies exactly as a real run does. It subtracts three things:

- It never stops a process. A rehearsal must not kill the worker it rehearses on (`gc_sweep.rs` `run_with_release`).
- It never prunes. Receipt retention is set to zero, because a rehearsal that prunes is not a rehearsal (`gc_sweep.rs` `run_with_release`).
- It subtracts the planned settle from the graph read. The report shows the outcome the real pass produces (`gc.rs` `gc_sweep`).

The `held` line is the trap. The line reads `held {id} (needs live stop: {reason})`. The condition is dry-run-only (`gc_sweep.rs` `run_with_release`). It fires on a claude row with no positive death evidence, such as a terminal roster state or a dead pid. So the rehearsal declines to promise a stop it cannot prove, and a real run can still take the row.

The guard exists because of one incident. On 2026-09-08 a dry run promised 9 retirements, and the real run retired 0 (`gc_sweep.rs` `needs_live_stop`).

The rehearsal has a second honest hold: `held {id} (dry-run did not evaluate: {gate}; apply may still refuse)`. A dry-run row whose remaining gate needs a mutation is named where it stands. It is never planted into `retired` or `pruned`, so neither count can read a promise the run did not evaluate (`gc_sweep.rs` `run_with_release`). The apply run can still refuse that gate: the dry-run report says `apply may still refuse`, naming the uncertainty.

If the dry run prints a `held` line, run the real verb and read its verdict.

## Rows no sweep can take

Three reasons are permanent by construction. The gate decides them before it reads the graph (`gc.rs` `gc_decide`), so they mask every later reason.

- `kept {id} (operator row)`: a human started this session (`gc.rs` `gc_decide`). No sweep touches it.
- `kept {id} (crowned)`: the row belongs to a crowned orchestrator (`gc.rs` `gc_decide`).
- `kept {id} (not a spawn row: {why})`: fno did not spawn the session. The row is someone else's fact about it. Done plus quiet does not make it fno's to remove (`gc.rs` `gc_decide`). One exit exists. A recorded pid that answers ESRCH proves a corpse. So does a claude row absent from a known `claude agents` roster read. Such a row falls through and is judged like any other row. An unknown or partial roster read keeps the row.

When the registry holds no origin at all, the third reason prints `no origin recorded` (`reap_render.rs` `render_reap`).

Do not act on these rows. A row quiet for 15 hours with no king is still permanent while its harness record answers for the session. The distinction matters: a row the sweep declined can leave on a later pass, while a row with a permanent exemption never leaves.

Measured on 2026-09-10: 4 of 51 judged rows carried `origin adopted` or `operator row`, and 4 more carried `crowned`. No sweep can take those 8 rows today or ever.

## The row is waiting on work

Four reasons mean real work still points at the row.

### open work

The full line reads `kept {id} (open work: {node} {status}; read via {reader})`. It names the first node that is not done, its status, and the source that resolved the link (`reap_render.rs` `render_reap`).

The `read via` field tells you how the sweep linked the row to the node:

- `sessions`: the graph dispatch records. This is the strong read.
- `registry`: the row node field in the registry.
- `name`: tokens 1 and 2 of the row name (`node_route.rs` `resolve`). A stale name can point at the wrong node.

If the node is genuinely open, run `fno backlog get <node>` and read the `status` field. The row waits for the node to ship. Deleting or hand-closing the node is falsifying work. It is never the fix.

Four facts free an open-node row anyway (`gc.rs` `gc_decide`):

- The harness published a terminal state for the session.
- A live newer row owns the same node.
- The node sits parked at `deferred` or `idea`.
- The node recorded merge status reads `merged`.

Each released row falls to the same quiet gate a done node takes, so the transcript still decides. A planner row never takes these four releases. It never keeps under open work while it carries assignments: its own reason names the planning lane (`gc.rs` `gc_decide`).

None of the four applies while the session drives an open PR. That keep outranks all four releases. The next section covers it.

The window is the newest keep on this row shape. An open node plus quiet inside `agents.reap.open_work_retire_s`, default 86400 s (`agents_config.rs` `DEFAULT_OPEN_WORK_RETIRE_SECS`), reads `kept {id} (open work inside the retire window: {node} {status}; read via {reader}; quiet past the window retires)`. The keep names the node pinning it, so an operator can act on the node rather than on the row. Past the window the row falls to the same grace gate a released row takes. An unresolved transcript has no clock to age past anything, so it keeps under the unchanged open-work reason (`gc.rs` `gc_decide`).

### open pr

The full line reads `kept {id} (open pr: {node} {detail})`. The session has a `do` row on an open node that carries `pr_number`, and the node's recorded `merge_status` is not `merged`. The PR is unmerged and this session is its driver, so retiring the row strands the PR with nothing left to drive it. The graph record names the candidate. The PR itself settles it. While the row sits inside the grace window the candidate holds and nothing is read. Once the row is quiet past the grace, the sweep asks GitHub once per pass, cached per PR (`gc_sweep.rs` `open_pr_verdict`). A PR still open holds. A merged or closed PR releases the row through the same quiet gate every finished session takes. An unreadable answer holds the row under `pr state contradicts`: a failed read never retires a row.

The keep outranks every release above it. A terminal roster state, a parked node, or a live newer peer that does not drive the PR leaves the row standing. Only a driving peer releases the row. Driving means the peer's session holds a `do` row on the node. A recorded `merge_status: merged` empties the keep. A merged PR is not an open one.

The remedy is merge, not reap. If another node must merge first, record a merge order (the next section shows the form). Otherwise drive the PR to merge. Run `fno do pr status <N>` to see what blocks it. A done node whose merge outcome nothing records reads GitHub once for the row's own PR. An unreadable read keeps the row too.

### the nudge ladder

A kept open-PR row is a session that is not driving. The daemon's retire arm runs a nudge ladder over every such row (`pr_nudge.rs` `run_ladder`). The rules, in order:

1. **Reset.** Transcript activity newer than the last nudge clears the budget: the session answered.
2. **Wait.** The transcript is inside the grace window, or the last nudge is too young.
3. **Pause.** A live merge order holds the session. The only allowed pause. A lead records it with `fno inbox decide "merge-order:<held-node>:after:<lead-node>" "<lead-node> merges first"`. The ladder waits while the lead node is not done.
4. **Escalate.** After 3 nudges with no activity, one operator question is filed on the marker `pr-nudge:`. The ladder then waits for activity.
5. **Mail.** A live session gets `fno agents mail send <full-session-id> "continue: PR #<N> on node <node> is kept open-pr; drive it to merge."`. The message carries the stdout line of `fno do pr status <N>`, so the session sees the verdict without a round trip.
6. **Resume.** A session with no live process gets `fno agents resume <full-session-id> --message "<text>"`, which relaunches the same conversation under its full session id.

Events: `pr_nudge_sent`, `pr_nudge_escalated`, `pr_nudge_paused`. State is one file per session under `~/.fno/pr-nudge/`. The ladder fires on the daemon arm only. `fno agents reap --dry-run` prints its plan as `would nudge {id} ({action})` and takes no effect.

### open do row on done node

The full line reads `kept {id} (open do row on done node: {node}: {detail})`. Every node the row names is done, but the graph still holds an open do row for the session. The retirement re-opens settled work, so the row stays (`gc_sweep.rs` `run_with_release`).

Two paths produce the line. The classify pass fires on the stale graph row (`gc_sweep.rs` `run_with_release`). The apply pass re-checks at stage and commit. Fresh work can arrive between decision and stop, and a stop then kills live work (`gc_sweep.rs` `commit_retirements`).

The real run carries its own cure. The settle pass fills stale open do rows on done and merged nodes before the row pass reads the graph (`gc.rs` `gc_sweep`, `gc_sweep.rs` `settle_stale_do_rows`). A dry run prints the cure as `would settle {id} (stale open do row filled on done+merged node: {node})`.

An additional PR settles by record, never by guess. One of three facts settles an extra. Its own stamp reads `merged` or `closed`. It is the primary of a node whose merge status reads `merged`. Another node carries it as its primary, and that node's worker holds the PR. The sweep reads GitHub once per pass for any extra that no fact settles. When the entry carries no url, the read resolves in the node's `cwd`. A merged or closed answer is stamped onto the entry. The stamp settles the row on the same pass. A refused stamp or an unreadable read keeps the hold.

Never hand-close the node to clear this line. That hides the obligation and falsifies the record.

### planning assignment not finished by this session

The line reads `kept {id} (planning assignment not finished by this session: {node})`. The row is a planner, the node sits planning-complete, and the session holds neither finished marker.

Marker one: the session's own blueprint or think row on the node carries a non-empty `ended_at` (`gc.rs` `gc_decide`).

Marker two: the session wrote the node's plan. The node's `plan_path` names a plan file that exists, and no other planner's row on the node started earlier (`gc_sweep.rs` `read_graph_entries`).

Two facts finish an assignment with neither marker. A node whose status reads `deferred` or `superseded` has moved on: nothing is left to plan. A halted planner's latest inside-leg report reads `done`. Its turn ended with no plan on the node, and it waits on nothing (`gc.rs` `gc_decide`). A planner waiting on input reads `blocked`, and a mid-turn planner reads `working`, and both keep the hold.

A finished planner retires once its transcript is quiet for 1200 s, not the default grace (`gc.rs` `PLANNING_IDLE_RETIRE_SECS`).

A planner with neither marker keeps its row, and the hold ages. Past `agents.hold_escalate_after_s` the line names the cure: `fno agents reap --release <row>` (`reap_render.rs` `render_reap`).

The retirement basis names the marker that fired, in order (`gc_sweep.rs` `run_with_release`): `planning finished on {node}: closed by this session`, `planning finished on {node}: plan written`, `planning finished on {node}: node {status}` for a moved-on node, or `planning finished on {node}: released`. A halted planner reads `planning halted on {node}: turn ended with no plan`. A blueprint row whose assignment set is empty retires through the session arms instead, and never borrows the planning wording.

### live descendant

The line reads `kept {id} (live descendant: {child})`. A live CHILD registry row names this row as its parent (`spawn_edge.rs` `live_child_of`). A CHILD is a join worker (`jn-t-`, legacy `j-`) or a row a crowned session spawned. A handoff, such as a blueprint's target or an advance dispatch, is a PEER and never holds its spawner. The parent stays until the child is gone, unless the parent's own harness reports `done`, `stopped` or `failed`. A terminal parent has no running surface for its children to keep alive, so the lineage guard yields to it. To wait on work inside your own feature, use a subagent, not a spawned row. Retire the child and the parent becomes eligible.

## The row is waiting on evidence

Each reason in this section is the sweep reading evidence, or refusing to guess from its absence.

### transcript unresolved

The line reads `kept {id} (transcript unresolved for {age}: absence is not quiet)`. The transcript probe answered nothing (`gc.rs` `grace_gate`). Four different facts produce the same line (`gc_inventory.rs` `census`):

- The row carries no harness session id.
- The harness is neither `claude` nor `codex`, so no readable store exists for it. A gemini or opencode row reads exactly this way.
- The ambient store root was unreadable on this pass. This is a torn read.
- The session id matched no file in any indexed store root.

To tell them apart, run `fno agents list` and read the `HARNESS` column first. Run the dry run again to clear a torn read. If the id and the harness both look right, search the store roots for the session id.

A reader who assumes the last cause is the only cause deletes a row that is merely on another harness. That is why all four causes are listed.

### no provenance

The line reads `kept {id} (no provenance: no source resolved a node (sessions, registry, name, transcript))`. No declared source resolved a node (`gc.rs` `gc_decide`). The sessions join answered nothing, the registry node field is empty, the name carries no node token, and the transcript mentions none.

The keep is no longer forever. When its inside-leg report reads done and fno never stopped the row, the row takes the quiet gate. Quiet past the grace retires it. A row still working, blocked, or stopped by fno keeps. Every kept row now carries a hold with an age, so `fno agents reap --release` and the escalation clock reach it.

Run `fno-agents node-route --names <name> --json` to read the cascade verdict for one row name.

### sources disagree

The line reads `kept {id} (sources disagree: {a} vs {b})`. Two sources resolved different nodes, and witnesses that disagree are not evidence (`node_route.rs` `resolve`).

The reading is not guessable. `{a}` is the source that disagreed. `{b}` is the node that source named, not the row's own node. The line `sources disagree: registry vs <node>` means the registry field names `<node>` and another source names a different one.

The same `fno-agents node-route` command confirms it. Fix the source that answered wrong, and the row becomes eligible on a later sweep.

### pr state contradicts

The line reads `kept {id} (pr state contradicts: {node} {detail})`. The node reads done, but PR evidence disagrees (`gc_sweep.rs` `open_pr_verdict`). The detail names the shape: `additional_prs: N of M not recorded merged`, or `merge_status: X` for a recorded status that is not `merged`.

The asymmetry matters (`gc.rs` `gc_decide`). A recorded status that is not `merged` holds the row. An absent status does not hold, because absence has three explanations and none of them is `unmerged`. The same stamp pass can write `closed` onto an additional PR. A closed stamp settles the entry the same way a merged one does.

### active

The line reads `kept {id} (active: transcript written {age}s ago)`. The transcript was written inside the grace window, which defaults to 900 seconds (`agents_config.rs` `DEFAULT_RETIRE_GRACE_SECS`, `gc.rs` `grace_gate`). The session is live in the only sense the law allows. Wait past the window. Two facts override it early: a terminal harness state (`done`, `stopped`, `failed`) and a provably dead pid (ESRCH). If `claude agents` reads the session `done` while the reaper prints this line, that combination is a defect, not a wait.

### probe unread

The line reads `kept {id} (probe unread: {detail})`. The fresh re-read of the row's transcript age did not answer inside its bound, so the row is neither known quiet nor known active. An unread instrument is never reported as `active`, and the row never carries an invented age of 0.

The detail names the in-process witness that answered instead. The registry row's `inside_leg.seq` is the quiet witness: the daemon writes it in process on every turn report, so no subprocess can time it out. `no inside-leg report on the row` means the row carries no report at all, so the witness cannot speak. `inside-leg seq moved {old} -> {new}` means a new turn report landed between classification and the re-read: the session woke up, and the keep is real. `a live working report is on the row` means the last report reads `working` and its ttl agrees, a turn plausibly in flight, never quiet, until the badge ages out. When the seq is unchanged and the classification age measured quiet, the row retires, the witness answers where the probe starved (`gc_sweep.rs` `run_with_release`).

The cure is patience, not a ruling: the next sweep's probe usually answers, and the row retires then.

### graph unreadable

The line reads `kept {id} (graph unreadable: never a retirement on a failed read)`. The graph read failed this pass (`gc_sweep.rs` `run_with_release`). Rerun the dry run. When the graph reads, the reason clears.

### no resumable receipt

The line reads `kept {id} (no resumable receipt: {reason})`. Every removal needs its recovery record on disk before any effect fires (`gc_sweep.rs` `commit_retirements`). The receipt failed to stage or persist, so the row stays. The line names the reason. Rerun the sweep.

## The row retired and its tree stayed

Tree lines start with `kept tree`. They are decided only after the row verdict is retire (`gc.rs` `tree_action`). A `kept tree` line never means the row survived. The row is gone, and only its checkout remains.

- `kept tree {id} (dirty: {path})`: uncommitted or untracked content waits for a human (`gc.rs` `tree_action`).
- `kept tree {id} (clean but the branch never merged: {path})`: real but unmerged work waits for a human judgment (`gc.rs` `tree_action`).
- `kept tree {id} (the cleanliness probe could not answer: {path})`: unknown never removes (`gc.rs` `tree_action`).
- `kept tree {id} (shared with {holder}, still live)`: another live row occupies the path (`gc_sweep.rs` `run_with_release`).

A clean and merged tree prunes, and the branch stays (`gc.rs` `tree_action`). The removal contract for worktrees lives in [../.claude/rules/worktrees.md](../.claude/rules/worktrees.md).

## The sweep tried and did not finish

Three lines mean the sweep acted on a retire decision and hit a refusal.

- `kept {id} (stop refused: {reason})`: the confirmed stop refused, so the row stays for retry on the next pass (`gc_sweep.rs` `run_with_release`). On a claude row with no death evidence, the reason names the missing evidence. After `claude stop` exits 0, the arm polls two witnesses for up to 15 s (`gc_claude_stop.rs` `stop_claude_confirmed`). The witnesses are the daemon roster and the `claude agents` state. A supervisor that tears the session down late no longer wedges the row for a whole tick.
- `prune failed {id} ({reason})`: the tree removal did not confirm (`gc_sweep.rs` `RetireRefusal`).
- `settle refused {node} ({reason})`: the settle write refused, named and never silent (`gc_sweep.rs` `settle_stale_do_rows_with`).

Two receipt lines ride the same retention pass. `expired receipt {name}` names a receipt the retention window retired. `kept receipt {name} ({reason})` names a failed read, and a failed read is not evidence of age (`gc_sweep.rs` `commit_retirements`).

## Edge cases that cost a session real time

Each item here cost a real session time. One corrects an older belief.

1. **`open work` is not a backlog chore.** The row waits for its node to ship. Deleting or hand-closing the node is falsifying work.
2. **A second claude account holds rows the ambient account cannot see.** `no job matching <id>` from one root is a wrong-root absence.
3. **`claude rm` takes the short id.** The full session id answers `No job matching` and exits 0. The exit code reads as success.
4. **`claude rm` refuses a session whose cwd has uncommitted changes.** In a canonical checkout that cwd never goes clean.
5. **A keeper pane reads dead to a fresh server.** Its child lives. Death evidence decides the stop (`gc_sweep.rs` `claude_death_reason`).
6. **A daemon older than the installed binary reports every status as `unknown`.** Unknown keeps rows. Run `fno agents list` and read its warning. Measured on 2026-09-10: one restart turned 57 unknown rows into 26 orphaned, 16 writing, 12 quiet, and 2 parked.
7. **The mux tab prune is scheduled, not manual-only.** An older belief held that nothing scheduled it. The manual verb adds `--include-used-shells` (`client.rs` `run_reap`). The daemon retire arm uses default flags (`gc.rs` `maybe_retirement_sweep`). `--no-mux` skips it on the manual verb.
8. **A row refused on every pass stays, and the report names the refusal each time.** Older builds discarded the reason. Today the render carries it (`reap_render.rs` `render_reap`).

## Every JSON key

Every top-level key of `fno agents reap --json`, one row each. The dry run renders the same object with `dry_run` true. `render_reap` emits the summary keys (`reap_render.rs` `render_reap`). `render_reap_with_inventory` splices two more, `inventory` and `mux` (`reap_render.rs` `render_reap_with_inventory`). One JSON read carries the verdicts and the world they were judged against.

| Key | Report line it feeds | Explained at |
|---|---|---|
| `retired` | `retired {id} ({basis})` and the count line | [In what order](#in-what-order) |
| `pruned` | `pruned {id} (clean and merged: {path})` | [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) |
| `prune_failed` | `prune failed {id} ({reason})` | [The sweep tried and did not finish](#the-sweep-tried-and-did-not-finish) |
| `settled_do_rows` | `settled {id} (stale open do row filled on done+merged node: {node})` | [open do row on done node](#open-do-row-on-done-node) |
| `settle_refused` | `settle refused {node} ({reason})` | [open do row on done node](#open-do-row-on-done-node) |
| `kept_operator` | `kept {id} (operator row)` | [Rows no sweep can take](#rows-no-sweep-can-take) |
| `kept_crowned` | `kept {id} (crowned)` | [Rows no sweep can take](#rows-no-sweep-can-take) |
| `kept_not_spawn` | `kept {id} (not a spawn row: {why})` | [Rows no sweep can take](#rows-no-sweep-can-take) |
| `kept_no_provenance` | `kept {id} (no provenance: ...)` | [no provenance](#no-provenance) |
| `kept_node_conflict` | `kept {id} (sources disagree: {a} vs {b})` | [sources disagree](#sources-disagree) |
| `kept_pr_contradicts` | `kept {id} (pr state contradicts: {node} {detail})` | [pr state contradicts](#pr-state-contradicts) |
| `kept_open_work` | `kept {id} (open work: {node} {status}; read via {reader})` | [open work](#open-work) |
| `kept_open_work_stale` | `kept {id} (open work inside the retire window: {node} {status}; read via {reader}; quiet past the window retires)` | [open work](#open-work) |
| `kept_open_do_row` | `kept {id} (open do row on done node: {node}: {detail})` | [open do row on done node](#open-do-row-on-done-node) |
| `kept_open_pr` | `kept {id} (open pr: {node} {detail})` | [open pr](#open-pr) |
| `kept_planning_unclosed` | `kept {id} (planning assignment not finished by this session: {node})` | [planning assignment not finished by this session](#planning-assignment-not-finished-by-this-session) |
| `kept_active` | `kept {id} (active: transcript written {age}s ago)` | [active](#active) |
| `kept_probe_unread` | `kept {id} (probe unread: {detail})` | [probe unread](#probe-unread) |
| `kept_transcript_unresolved` | `kept {id} (transcript unresolved for {age}: absence is not quiet)` | [transcript unresolved](#transcript-unresolved) |
| `kept_graph_unreadable` | `kept {id} (graph unreadable: never a retirement on a failed read)` | [graph unreadable](#graph-unreadable) |
| `kept_dirty` | `kept tree {id} (dirty: {path})` | [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) |
| `kept_unmerged` | `kept tree {id} (clean but the branch never merged: {path})` | [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) |
| `kept_unprobed` | `kept tree {id} (the cleanliness probe could not answer: {path})` | [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) |
| `kept_shared_tree` | `kept tree {id} (shared with {holder}, still live)` | [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) |
| `kept_live_descendants` | `kept {id} (live descendant: {child})` | [live descendant](#live-descendant) |
| `stop_refused` | `kept {id} (stop refused: {reason})` | [The sweep tried and did not finish](#the-sweep-tried-and-did-not-finish) |
| `needs_live_stop` | `held {id} (needs live stop: {reason})` | [A dry run and a real run answer different questions](#a-dry-run-and-a-real-run-answer-different-questions) |
| `dry_run_unverified` | `held {id} (dry-run did not evaluate: {gate}; apply may still refuse)` | [A dry run and a real run answer different questions](#a-dry-run-and-a-real-run-answer-different-questions) |
| `kept_no_receipt` | `kept {id} (no resumable receipt: {reason})` | [The sweep tried and did not finish](#the-sweep-tried-and-did-not-finish) |
| `expired_receipts` | `expired receipt {name}` | [The sweep tried and did not finish](#the-sweep-tried-and-did-not-finish) |
| `kept_receipts` | `kept receipt {name} ({reason})` | [The sweep tried and did not finish](#the-sweep-tried-and-did-not-finish) |
| `holds` | projection, no line: one entry per held handle; feeds the `retire_holds` journal row | [Read the answer](#read-the-answer) |
| `hold_escalate_after_s` | projection, no line: the threshold behind the escalation suffix `; past ...: fno agents reap --release` | [planning assignment not finished by this session](#planning-assignment-not-finished-by-this-session) |
| `release_refused` | the refusal lines a `--release` pass prints verbatim | [Rows no sweep can take](#rows-no-sweep-can-take) |
| `open_pr_rows` | projection, no line: one entry per kept open-PR row; the nudge ladder reads it | [the nudge ladder](#the-nudge-ladder) |
| `open_pr_nudge` | `would nudge {id} ({action})` | [the nudge ladder](#the-nudge-ladder) |
| `schema_skew` | `registry schema v{on_disk} is ahead of the v{understood} this fno understands: ...` | [Read the answer](#read-the-answer) |
| `dry_run` | the `(dry-run: no changes made)` marker | [A dry run and a real run answer different questions](#a-dry-run-and-a-real-run-answer-different-questions) |
| `inventory` | projection, no line: the census of sessions and store roots this pass read | [Read the answer](#read-the-answer) |
| `mux` | `mux sweep (ran|unread|skipped by --no-mux)` | [Ten programs stop or remove a session](#ten-programs-stop-or-remove-a-session) |

## One table: the reason, the act, the check

Every keep and hold reason from the sections above, one row each.

| Report line | Act or wait | The check that confirms it |
|---|---|---|
| `kept {id} (operator row)` | Wait. This one is permanent. | The line itself names the reason. |
| `kept {id} (crowned)` | Wait. This one is permanent. | The line itself names the reason. |
| `kept {id} (not a spawn row: {why})` | Wait. This one is permanent. | The line prints `origin adopted` or `no origin recorded`. |
| `kept {id} (open work: {node} {status}; read via {reader})` | Wait for the node to ship. Never close the node by hand. | Run `fno backlog get <node>` and read `status`. |
| `kept {id} (open work inside the retire window: {node} {status}; read via {reader}; quiet past the window retires)` | Wait past the window, or act on the named node. | Run `fno backlog get <node>` and read `status`. |
| `kept {id} (open pr: {node} {detail})` | Drive the PR to merge, or record a merge order. | Run `fno do pr status <N>`. |
| `kept {id} (open do row on done node: {node}: {detail})` | Wait. A real run settles it. Never close the node. | Run `fno backlog get <node>` and read `status`. |
| `kept {id} (planning assignment not finished by this session: {node})` | Wait up to 20 quiet minutes, or rule with `fno agents reap --release <row>` once escalated. | The line names the node and the hold age. |
| `kept {id} (live descendant: {child})` | Wait for the live CHILD row to go, unless the parent's roster state reads `done`, `stopped` or `failed`. A handoff row (`sob-t-`, `ac-t-`) never holds its spawner. | The same report carries the child line. |
| `kept {id} (active: transcript written {age}s ago)` | Wait past the grace window. A terminal harness state or a dead pid retires the row early, unless the row keeps for an open PR. | Run the dry run again. Read the new age. |
| `kept {id} (probe unread: {detail})` | Wait for the next sweep. The probe usually answers then. A moved seq means a new turn: the keep is real. | Run the dry run again. Read the detail. |
| `kept {id} (transcript unresolved for {age}: absence is not quiet)` | Diagnose one of the four causes above. | Run `fno agents list`. Rerun the dry run. Search the store roots. |
| `kept {id} (no provenance: ...)` | Restore one resolvable source for the row. | Run `fno-agents node-route --names <name> --json`. |
| `kept {id} (sources disagree: {a} vs {b})` | Fix source `{a}`, which named `{b}`. | Run `fno-agents node-route --names <name> --json`. |
| `kept {id} (pr state contradicts: {node} {detail})` | Fix the PR record named in `{detail}`. | Run `fno backlog get <node>`. |
| `kept {id} (graph unreadable: ...)` | Rerun when the graph reads. | Run the dry run again. |
| `kept {id} (no resumable receipt: {reason})` | Rerun the sweep. | The line names the reason. |
| `held {id} (needs live stop: {reason})` | Run the real verb. The row can still retire. | Run `fno agents reap`. |
| `held {id} (dry-run did not evaluate: {gate}; apply may still refuse)` | Run the real verb. The apply can still refuse the named gate. | Run `fno agents reap`. |
| `kept {id} (stop refused: {reason})` | Read the reason. The sweep retries next pass. | Run the dry run again. |
| `kept tree {id} (dirty: {path})` | Judge the tree. The row is already gone. | Run `git status` in `{path}`. |
| `kept tree {id} (clean but the branch never merged: {path})` | Judge the branch. The tree waits. | Run `git log` on the branch. |
| `kept tree {id} (the cleanliness probe could not answer: {path})` | Rerun the sweep. | Run the dry run again. |
| `kept tree {id} (shared with {holder}, still live)` | Wait for the holder row to go. | The holder appears in the same report. |
| `prune failed {id} ({reason})` | Read the reason. | Run the sweep again. |
| `settle refused {node} ({reason})` | Read the reason. | Run the sweep again. |
| `expired receipt {name}` | Nothing. Retention ran. | The line names the receipt. |
| `kept receipt {name} ({reason})` | Nothing. A failed read is not age. | The line names the reason. |

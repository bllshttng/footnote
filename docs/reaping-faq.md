# Reaping FAQ: why is this row still here?

A reaping sweep judged a session row and kept it. This page names the keep reason and tells you what to do. It covers the three sweep programs, every keep reason, and the checks that tell them apart.

Run `fno agents reap --dry-run` and find your row handle in the report. The dry run classifies every row, names one reason per row, and writes nothing. It stops no process, prunes no tree, and writes no receipt.

Measured on this machine on 2026-09-10 with the dry run: 1 `would retire` line, 49 `kept` lines, 1 `held` line. The largest bucket was `open work` with 22 rows. Eight rows sat behind a permanent exemption: 2 operator rows, 4 crowned rows, 2 adopted rows. Counts like these move within the hour, so run your own dry run and date the result.

## Is this page for you?

A reaping sweep kept a session row and you want to know why, or you want a row gone and reap refuses. This page owns the keep reasons, the three sweep programs, and the checks that tell them apart. Misreading it makes you force a delete the machine will re-judge on the next sweep, or kill a worker whose row was telling the truth.

Not for: the worktree removal contract (which trees prune on merge and which never do). That is answered at [The row retired and its tree stayed](#the-row-retired-and-its-tree-stayed) and owned by [../.claude/rules/worktrees.md](../.claude/rules/worktrees.md).

## Read the answer

Every report line has three parts: the verb, the row handle, and the reason in parentheses.

```
kept fb6399d5 (not a spawn row: origin adopted)
```

The verb names the verdict. `kept` means the sweep declined this pass. `held` means the rehearsal declined to promise an action. `would retire` is the dry-run remove verdict, and `retired` is the real one.

The first report line carries the counts. The last lines carry the dry-run marker and the mux sweep line. A retire line names its basis, for example `every named node done`, with the witnesses that agreed.

## Three programs answer to the word reap

Three programs share the word reap. A reader who watches one and concludes the others are broken has mixed them up. This exact confusion cost a real session. The arms readout showed `acted=0 skip=held` while the manual verb retired 3 rows in the same minute.

| Program | Entry point | Trigger | Tick name |
|---|---|---|---|
| The manual verb | `fno agents reap` | a person types it | none |
| The registry sweep arm | the daemon idle tick | interval `agents.retire_interval_s`, default grace/3 | `retire` |
| The merge-request arm | the daemon, after a merge | a recorded merge cleanup request | `reap` |

The manual verb and the registry arm run the same sweep body (`client.rs:2386`, `gc.rs:833`). The registry arm runs one sweep at a time behind a one-in-flight gate (`gc.rs:821`). A slow sweep holds the next request, so the effective cadence is not the interval. Run `fno-agents status` to read the arms table.

The merge-request arm is a different program (`merge_reap.rs:669-775`). It loops only over pending merge cleanup requests (`merge_reap.rs:703`). With no pending request it does nothing. Its floor is 60 seconds (`merge_reap.rs:36`), and a request older than 86400 seconds expires unacted (`merge_reap.rs:42`). Its skip reasons are `no_requests`, `all_in_grace`, and `held` (`merge_reap.rs:761-769`), and its detail line carries the held count beside the request count. So its `acted=0 skip=held` line says nothing about the registry sweep.

The mux sideline sweep rides the manual verb only (`client.rs:2408-2415`). It shells out to `fno mux workspace prune` (`client.rs:2432-2444`). Neither daemon arm runs it. Pass `--no-mux` to skip it. So only a person typing the verb runs the sweep for ghost mux panes.

A fourth tool answers to a related name. `fno-agents roster-reap` removes claude rows that fno never registered. Nothing schedules it. A person types it. It is a dry run by default, and `--apply` acts (`client.rs:2470-2494`).

## A dry run and a real run answer different questions

A dry run classifies exactly as a real run does. It subtracts three things:

- It never stops a process. A rehearsal must not kill the worker it rehearses on (`gc_sweep.rs:1188-1191`).
- It never prunes. Receipt retention is set to zero, because a rehearsal that prunes is not a rehearsal (`gc.rs:500`).
- It subtracts the planned settle from the graph read, so the report shows the outcome the real pass produces (`gc.rs:485-490`).

The `held` line is the trap. The line reads `held {id} (needs live stop: {reason})`. The condition is dry-run-only (`gc_sweep.rs:1229`). It fires on a claude row with no positive death evidence, such as a terminal roster state or a dead pid. So the rehearsal declines to promise a stop it cannot prove, and a real run can still take the row.

The guard exists because of one incident. On 2026-09-08 a dry run promised 9 retirements, and the real run retired 0 (`gc_sweep.rs:115-121`).

If the dry run prints this `held` line, run the real verb and read its verdict.

## Rows no sweep can take

Three reasons are permanent by construction. The gate decides them before it reads the graph (`gc.rs:222-236`), so they mask every later reason.

- `kept {id} (operator row)`: a human started this session (`gc.rs:135-136`). No sweep touches it.
- `kept {id} (crowned)`: the row belongs to a crowned orchestrator (`gc.rs:137-138`).
- `kept {id} (not a spawn row: origin adopted)`: fno did not spawn the session. The row is someone else's fact about it, and done plus quiet does not make it fno's to remove (`gc.rs:229-236`).

When the registry holds no origin at all, the third reason prints `no origin recorded` (`reap_render.rs:216-220`).

Do not act on these rows. A row quiet for 15 hours with no king is still permanent. The distinction matters: a row the sweep declined can leave on a later pass, while a row with a permanent exemption never leaves.

Measured on 2026-09-10: 4 of 51 judged rows carried `origin adopted` or `operator row`, and 4 more carried `crowned`. No sweep can take those 8 rows today or ever.

## The row is waiting on work

Four reasons mean real work still points at the row.

### open work

The full line reads `kept {id} (open work: {node} {status}; read via {reader})`. It names the first node that is not done, its status, and the source that resolved the link (`gc_sweep.rs:86-89`).

The `read via` field tells you how the sweep linked the row to the node:

- `sessions`: the graph dispatch records. This is the strong read.
- `registry`: the row node field in the registry.
- `name`: tokens 1 and 2 of the row name (`node_route.rs:256-281`). A stale name can point at the wrong node.

If the node is genuinely open, run `fno backlog get <node>` and read the `status` field. The row waits for the node to ship. Deleting or hand-closing the node is falsifying work. It is never the fix.

Four facts free an open-node row anyway (`gc.rs:106-112`):

- The harness published a terminal state for the session.
- A live newer row owns the same node.
- The node sits parked at `deferred` or `idea` (`gc.rs:128`).
- The node recorded merge status reads `merged`.

Each released row falls to the same quiet gate a done node takes, so the transcript still decides.

### open do row on done node

The full line reads `kept {id} (open do row on done node: {node})`. Every node the row names is done, but the graph still holds an open do row for the session. The retirement re-opens settled work, so the row stays (`gc.rs:172-175`).

Two paths produce the line. The classify pass fires on the stale graph row (`gc_sweep.rs:1044-1050`). The apply pass re-checks before any effect, because fresh work can arrive between decision and stop, and a stop then kills live work (`gc_sweep.rs:1473-1502`).

The real run carries its own cure. The settle pass fills stale open do rows on done and merged nodes before the row pass reads the graph (`gc.rs:452-456`). A dry run prints the cure as `would settle {id} (stale open do row filled on done+merged node: {node})`.

Never hand-close the node to clear this line. That hides the obligation and falsifies the record.

### planning assignment never closed by this session

The row is a planner. Every node it was assigned reached a planning-complete status, but this session's own blueprint or think row on the node carries no `ended_at` (`gc.rs:157-161`). The completion belongs to an earlier assignment, so this quiet replanning worker keeps its row (`gc.rs:264-283`).

### live descendant

The line reads `kept {id} (live descendant: {child})`. A live registry row names this row as its parent (`gc_sweep.rs:1681-1694`). The parent stays until the child is gone, unless the parent's own harness reports `done`, `stopped` or `failed` (`gc_sweep.rs:1684`). A terminal parent has no running surface for its children to keep alive, so the lineage guard yields to it. Retire the child and the parent becomes eligible.

## The row is waiting on evidence

Each reason in this section is the sweep reading evidence, or refusing to guess from its absence.

### transcript unresolved

The line reads `kept {id} (transcript unresolved: absence is not quiet)`. The transcript probe answered nothing (`gc.rs:329`). Four different facts produce the same line (`gc_inventory.rs:118-163`):

- The row carries no harness session id (`gc_inventory.rs:119`).
- The harness is neither `claude` nor `codex`, so no readable store exists for it (`gc_inventory.rs:121-126`). A gemini or opencode row reads exactly this way.
- The ambient store root was unreadable on this pass. This is a torn read (`gc_inventory.rs:148-150`).
- The session id matched no file in any indexed store root.

To tell them apart, run `fno agents list` and read the `HARNESS` column first. Run the dry run again to clear a torn read. If the id and the harness both look right, search the store roots for the session id.

A reader who assumes the last cause is the only cause deletes a row that is merely on another harness. That is why all four causes are listed.

### no provenance

The line reads `kept {id} (no provenance: no source resolved a node (sessions, registry, name, transcript))`. No declared source resolved a node (`gc.rs:142-147`). The sessions join answered nothing, the registry node field is empty, the name carries no node token, and the transcript mentions none.

Run `fno-agents node-route --names <name> --json` to read the cascade verdict for one row name.

### sources disagree

The line reads `kept {id} (sources disagree: {a} vs {b})`. Two sources resolved different nodes, and witnesses that disagree are not evidence (`node_route.rs:338-357`).

The reading is not guessable. `{a}` is the source that disagreed. `{b}` is the node that source named, not the row's own node (`gc_sweep.rs:824-830`). The line `sources disagree: registry vs <node>` means the registry field names `<node>` and another source names a different one.

The same `fno-agents node-route` command confirms it. Fix the source that answered wrong, and the row becomes eligible on a later sweep.

### pr state contradicts

The line reads `kept {id} (pr state contradicts: {node} {detail})`. The node reads done, but PR evidence disagrees (`gc_sweep.rs:833-857`). The detail names the shape: `additional_prs: N of M not recorded merged`, or `merge_status: X` for a recorded status that is not `merged`.

The asymmetry matters (`gc.rs:152-155`). A recorded status that is not `merged` holds the row. An absent status does not hold, because absence has three explanations and none of them is `unmerged`.

### active

The line reads `kept {id} (active: transcript written {age}s ago)`. The transcript was written inside the grace window, which defaults to 900 seconds (`agents_config.rs:349`, `gc.rs:334-349`). The session is live in the only sense the law allows. Wait past the window. Two facts override it early: a terminal harness state (`done`, `stopped`, `failed`) and a provably dead pid (ESRCH). If `claude agents` reads the session `done` while the reaper prints this line, that combination is a defect, not a wait.

### graph unreadable

The line reads `kept {id} (graph unreadable: never a retirement on a failed read)`. The graph read failed this pass (`gc.rs:169-171`). Rerun the dry run. When the graph reads, the reason clears.

### no resumable receipt

The line reads `kept {id} (no resumable receipt: {reason})`. Every removal needs its recovery record on disk before any effect fires (`gc_sweep.rs:1503-1518`). The receipt failed to stage or persist, so the row stays. The line names the reason. Rerun the sweep.

## The row retired and its tree stayed

Tree lines start with `kept tree`. They are decided only after the row verdict is retire (`gc.rs:340-342`). A `kept tree` line never means the row survived. The row is gone, and only its checkout remains.

- `kept tree {id} (dirty: {path})`: uncommitted or untracked content waits for a human (`gc.rs:208-209`).
- `kept tree {id} (clean but the branch never merged: {path})`: real but unmerged work waits for a human judgment (`gc.rs:210-212`).
- `kept tree {id} (the cleanliness probe could not answer: {path})`: unknown never removes (`gc.rs:213-214`).
- `kept tree {id} (shared with {holder}, still live)`: another live row occupies the path (`gc_sweep.rs:60-63`).

A clean and merged tree prunes, and the branch stays (`gc.rs:206-207`). The removal contract for worktrees lives in [../.claude/rules/worktrees.md](../.claude/rules/worktrees.md).

## The sweep tried and did not finish

Three lines mean the sweep acted on a retire decision and hit a refusal.

- `kept {id} (stop refused: {reason})`: the confirmed stop refused, so the row stays for retry on the next pass (`gc_sweep.rs:112-114`). On a claude row with no death evidence, the reason names the missing evidence (`gc_sweep.rs:1256-1267`).
- `prune failed {id} ({reason})`: the tree removal did not confirm (`gc_sweep.rs:55-59`).
- `settle refused {node} ({reason})`: the settle write refused, named and never silent (`gc_sweep.rs:103-104`).

Two receipt lines ride the same retention pass. `expired receipt {name}` names a receipt the retention window retired. `kept receipt {name} ({reason})` names a failed read, and a failed read is not evidence of age (`gc_sweep.rs:126-130`).

## Edge cases that cost a session real time

Each item here cost a real session time. One corrects an older belief.

1. **`open work` is not a backlog chore.** The row waits for its node to ship. Deleting or hand-closing the node is falsifying work.
2. **A second claude account holds rows the ambient account cannot see.** `no job matching <id>` from one root is a wrong-root absence.
3. **`claude rm` takes the short id.** The full session id answers `No job matching` and exits 0. The exit code reads as success.
4. **`claude rm` refuses a session whose cwd has uncommitted changes.** In a canonical checkout that cwd never goes clean.
5. **A keeper-held pane reads dead to a fresh server while its child lives.** Positive death evidence decides a stop (`gc.rs:330-333`).
6. **A daemon older than the installed binary reports every status as `unknown`.** Unknown keeps rows. Run `fno agents list` and read its warning. Measured on 2026-09-10: one restart turned 57 unknown rows into 26 orphaned, 16 writing, 12 quiet, and 2 parked.
7. **The mux sweep runs only on the manual verb.** An older belief held that nothing scheduled it. Today the verb runs it (`client.rs:2408-2415`), and neither daemon arm does.
8. **A row refused on every pass stays, and the report names the refusal each time.** Older builds discarded the reason. Today the render carries it (`reap_render.rs:289-291`).

## One table: the reason, the act, the check

Every keep and hold reason from the sections above, one row each.

| Report line | Act or wait | The check that confirms it |
|---|---|---|
| `kept {id} (operator row)` | Wait. This one is permanent. | The line itself names the reason. |
| `kept {id} (crowned)` | Wait. This one is permanent. | The line itself names the reason. |
| `kept {id} (not a spawn row: {why})` | Wait. This one is permanent. | The line prints `origin adopted` or `no origin recorded`. |
| `kept {id} (open work: {node} {status}; read via {reader})` | Wait for the node to ship. Never close the node by hand. | Run `fno backlog get <node>` and read `status`. |
| `kept {id} (open do row on done node: {node})` | Wait. A real run settles it. Never close the node. | Run `fno backlog get <node>` and read `status`. |
| `kept {id} (planning assignment never closed by this session: {node})` | Wait for the session to close its own assignment. | The line names the node. |
| `kept {id} (live descendant: {child})` | Wait for the child row to go, unless the parent's roster state reads `done`, `stopped` or `failed`. | The same report carries the child line. |
| `kept {id} (active: transcript written {age}s ago)` | Wait past the grace window. A terminal harness state or a dead pid retires the row early. | Run the dry run again. Read the new age. |
| `kept {id} (transcript unresolved: absence is not quiet)` | Diagnose one of the four causes above. | Run `fno agents list`. Rerun the dry run. Search the store roots. |
| `kept {id} (no provenance: ...)` | Restore one resolvable source for the row. | Run `fno-agents node-route --names <name> --json`. |
| `kept {id} (sources disagree: {a} vs {b})` | Fix source `{a}`, which named `{b}`. | Run `fno-agents node-route --names <name> --json`. |
| `kept {id} (pr state contradicts: {node} {detail})` | Fix the PR record named in `{detail}`. | Run `fno backlog get <node>`. |
| `kept {id} (graph unreadable: ...)` | Rerun when the graph reads. | Run the dry run again. |
| `kept {id} (no resumable receipt: {reason})` | Rerun the sweep. | The line names the reason. |
| `held {id} (needs live stop: {reason})` | Run the real verb. The row can still retire. | Run `fno agents reap`. |
| `kept {id} (stop refused: {reason})` | Read the reason. The sweep retries next pass. | Run the dry run again. |
| `kept tree {id} (dirty: {path})` | Judge the tree. The row is already gone. | Run `git status` in `{path}`. |
| `kept tree {id} (clean but the branch never merged: {path})` | Judge the branch. The tree waits. | Run `git log` on the branch. |
| `kept tree {id} (the cleanliness probe could not answer: {path})` | Rerun the sweep. | Run the dry run again. |
| `kept tree {id} (shared with {holder}, still live)` | Wait for the holder row to go. | The holder appears in the same report. |
| `prune failed {id} ({reason})` | Read the reason. | Run the sweep again. |
| `settle refused {node} ({reason})` | Read the reason. | Run the sweep again. |
| `expired receipt {name}` | Nothing. Retention ran. | The line names the receipt. |
| `kept receipt {name} ({reason})` | Nothing. A failed read is not age. | The line names the reason. |

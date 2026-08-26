# Unified Loop Runtime

**Epic:** step 5, control-plane collapse
**Group 1 shipped:** runtime + target driver + exec shim
**Group 2 shipped:** megawalk driver
**Group 3:** planned - see "What lands later" below
**Sibling doc:** [control-plane-loop.md](control-plane-loop.md) - the stop-hook decision verb INSIDE a session

## Scope

This document covers the driver loop AROUND sessions: `crates/fno-agents/src/loop_runtime.rs`, `loop_target.rs`, `loop_dispatch.rs`, and the `scripts/run-target-loop.sh` exec shim. For the stop-hook decision verb that runs INSIDE each session, see [control-plane-loop.md](control-plane-loop.md).

The target stop shims resolve the manifest by harness session id before invoking `loop-check`. When a cwd manifest's `harness_session_id` or legacy alias names the stopping session, the shims keep it. Otherwise, `fno-agents manifest-for-session` scans the repository worktrees. A confirmed miss emits `loop-check: no manifest names session <id>; visitor allowed` and allows the visitor to stop. An unreadable resolver remains checker-unavailable and takes the shim's bounded retry path.

---

## The loop primitive

```rust
pub fn run_loop(
    queue:      &mut dyn Queue,
    dispatcher: &dyn Dispatcher,
    budget:     &LoopBudget,
    journal:    &Journal,
    cancel:     &dyn Fn() -> bool,
) -> Result<LoopOutcome, LoopError>
```

All drivers share one loop body. The driver supplies a `Queue` impl and a `Dispatcher` impl; the runtime has no opinion about what a "unit" is or how sessions are launched. This is the fifth-driver test: adding a new driver is a new trait impl, zero runtime change.

### Algorithm (abridged)

```
outer loop:
  check cancel -> Interrupted
  budget check via queue.has_pending()
                       -> pending: Budget / drained: NoWork
  unit = queue.next()   -> None: NoWork (terminate)
  resume guard: journal has termination for unit.session_key?
    yes -> close without dispatch (AC1-FR), journal node_closed,
           iterations_used += 1, continue
  inner loop:
    budget check         -> Budget (axis: "iterations")
    cancel check         -> Interrupted
    iterations_used += 1
    journal: loop_unit_dispatched
    session = dispatcher.run(unit)
    session.wait()
    journal has termination? -> close unit, break inner loop
    else: journal node_failed, re-dispatch
```

### Walk-level outcome

`LoopOutcome` carries the walk-level `TerminationReason`, total `iterations_used`, and a `Vec<UnitResult>` (per-unit evidence + close outcome). For the degenerate single-unit (target) walk, the headline reason shown to the caller is the unit's own evidence reason, not the walk-level `NoWork` that follows it - `NoWork` is plumbing there, not news.

---

## The three seams

### Queue

```rust
pub trait Queue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError>;
    fn has_pending(&mut self) -> Result<bool, LoopError>;
    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError>;
}
```

`has_pending` answers whether `next` can return a unit right now, without taking one. It must be side-effect free: no claim acquisition, no cursor advance. The outer budget check probes it before any dequeue. A unit the walk cannot afford is never taken. A claiming Queue acquires the claim in `next()`. A unit taken then dropped strands that claim.

`&mut self` is deliberate (locked decision F8): the group-2 megawalk Queue carries cursor state and consecutive-failure counters that are inherently sequential. Using `&self` would require pointless `Mutex`-wrapping for single-threaded walk state. `run_loop` is always called from one thread.

`close` returns `CloseOutcome`: `Closed`, `Refused(String)`, `Parked(String)`, or `AwaitingMerge`. The runtime journals none of these directly - it records `loop_terminated` at walk end; the Queue impl is responsible for any additional side-effects on close (e.g. `fno backlog done` in group 2). `AwaitingMerge` is success-shaped - the PR is up and reviewed but not yet merged, so the graph node stays `in_review` and `reconcile` closes it at the actual merge; the claim is released and no failure is recorded.

**Target Queue (group 1):** degenerate - one unit read from `.fno/target-state.md`. `close()` is inert: the session's own stop hook already emitted the `termination` event; the manifest is immutable. As of step 6 the session's terminal side-effects (ledger session-record, and on a ship the plan stamp/graduate + handoff artifact) are written by `fno-agents finalize`, which the shim invokes at the terminal-allow boundary BEFORE the worker process exits. A failed `DoneDelivery` finalize keeps the hook alive for an idempotent retry; legacy finalize failures remain best-effort. The outer loop only observes the terminal after the hook permits exit, so `TargetQueue::close` stays inert exactly as designed - placing the writes in `close` would miss attended interactive `/target` runs, which have no outer loop at all.

**Megawalk Queue (group 2):** shells `fno backlog next --json` and `fno backlog done`. It NEVER reads `graph.json` directly (locked decision / grilled 7). Selection logic (epics-first, project scoping, rank, `make_selection_sort_key`) stays inside `fno backlog next` - one place, no duplication.

### Dispatcher

```rust
pub trait Dispatcher {
    fn run(&self, unit: &Unit, ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError>;
}

pub trait Session {
    fn wait(&mut self) -> Result<i32, LoopError>;
}
```

Stateless with respect to the walk loop; any per-dispatch state is internal to the impl. `DispatchCtx` carries the 1-based `iteration` counter.

**ShelloutDispatcher (group 1, only step-5 impl):** sources `driver-<name>.sh` from `scripts/lib/` and calls `driver_invoke` via `bash -c`. The Rust side manages process lifecycle, env passthrough, and exit-code collection; it never reimplements driver behavior. The trait exists so a future daemon/PTY impl can be wired in as a drop-in replacement - same seam, no runtime change.

Signal death: when `session.wait()` returns a process killed by signal N, `ShelloutSession` returns `128 + N` (the shell convention: SIGTERM=15 -> 143, SIGKILL=9 -> 137). This value is recorded in the `node_failed` event's `exit_code` field.

### Journal

```rust
pub struct Journal { /* project_path: PathBuf, global_path: PathBuf */ }

impl Journal {
    pub fn new(project_path: ProjectJournalPath, global_path: GlobalJournalPath) -> Self;
    pub fn append(&self, event_type: &str, data: Value) -> Result<(), LoopError>;
    pub fn find_termination(&self, session_key: &str) -> Result<Option<Evidence>, LoopError>;
}
```

Two newtype wrappers prevent silent positional swap of the two same-type path arguments:

```rust
pub struct ProjectJournalPath(pub PathBuf);  // writes are FATAL
pub struct GlobalJournalPath(pub PathBuf);   // writes are best-effort
```

**Project journal (`<repo-root>/.fno/events.jsonl`) is authoritative.**

Worktrees symlink that path to the canonical checkout's journal, and a write failure there stops dispatch loudly (`LoopError::Journal`).

An unobservable walk that continues spending compute is worse than stopping.

**Global mirror (`~/.fno/events.jsonl`) is best-effort.** A write failure is logged to stderr and never propagated. The project journal is the record; the global mirror is convenience for cross-project tooling.

The method is named `append` (not `emit` or `emit_fields`) deliberately - see "Two-tier event model" below.

---

## The typed-event contract

The loop NEVER parses session stdout. The session's terminal state is communicated entirely through the project journal. When it decides to allow the session to exit, the stop hook (`fno-agents loop-check`) emits a `termination` event. `Journal::find_termination` scans for the last matching event keyed on `unit.session_key` (= `session_id` from the manifest).

Envelope shape for all loop-runtime events:

```json
{"ts":"2026-06-06T02:00:00Z","type":"<kind>","source":"loop","data":{...}}
```

`source: "loop"` is distinct from `source: "hook"` used by `loop-check`. Consumers that aggregate both streams use the source field to distinguish them.

### Loop-stream event kinds

| Event | Source | When |
|---|---|---|
| `loop_unit_dispatched` | loop | before each unit dispatch |
| `node_failed` | loop | session exits without a `termination` event (watchdog synthesis) |
| `loop_terminated` | loop | walk-level termination (NoWork, Budget, Interrupted) |

### Hook-stream event kinds (from `control-plane-loop.md`)

| Event | Source | When |
|---|---|---|
| `loop_check` | hook | every stop-hook fire |
| `termination` | hook | session allows exit (TerminationReason) |
| `loop_check_watch_idle` | hook | a `<watching>` Claude session idles non-terminally on a verified async wait (CI pending / bot review outstanding); the fire returns `allow` with `termination_reason: null` and extends the claim lease, so the session parks until its harness-tracked watcher fires instead of re-blocking every tick. Claude-only: a `FNO_DRIVER_LIB` loop-run child exits on allow, and codex/gemini have no self-wake on background-task exit (their daemon-consumer waker ships separately), so both keep today's block behavior. |
| `loop_check_gh_error` | hook | gh read fails during `done()` |
| `loop_advisory_mode` | hook | advisory-mode session |
| `loop_check_binary_missing` | hook | `fno-agents` binary not found |
| `loop_check_legacy_manifest` | hook | pre-wedge manifest detected |

### Two-tier event model (the #1 external-reviewer question)

The loop-stream kinds (`loop_unit_dispatched`, `node_failed`, `loop_terminated`) are defined in `events-schema.yaml` as target-stream events. They are deliberately NOT registered in `KNOWN_EVENT_KINDS` (in `lib.rs`) or in `events-v3.json`.

`KNOWN_EVENT_KINDS` / `events-v3.json` are the Branch-B daemon stream - the fno-agents PTY/IPC event bus. The parity scanner in `lib.rs` greps for `.emit(` call sites and requires each to appear in `KNOWN_EVENT_KINDS`. The loop runtime uses `Journal::append()` (not `.emit()`/`.emit_fields()`) precisely to opt out of that parity scan. These are two distinct event streams at two distinct altitudes; merging them would conflate walk-level orchestration records with per-agent IPC events.

---

## The degenerate walk (target driver)

Target = one unit, re-dispatch until a terminal event.

`TargetQueue::from_manifest` reads `.fno/target-state.md` and constructs a single `Unit`:

| Unit field | Source |
|---|---|
| `id` | `session_id` frontmatter field |
| `title` | `input` frontmatter field |
| `session_key` | same as `id` (matched against `termination.data.session_id`) |
| `plan_path` | `plan_path` frontmatter field (optional) |

After `next()` returns the unit and `close()` is called, subsequent `next()` calls return `None` - the outer loop exits with `NoWork`. The CLI reports the unit's evidence reason as the headline exit code, not the walk-level `NoWork`.

**Watchdog synthesis:** a dispatched session that exits without a matching `find_termination` event gets a `node_failed` event with its exit code (including `128+N` for signal deaths). The runtime then re-dispatches on the next inner iteration.

**Iteration ceiling -> Budget:** the walk terminates with `TerminationReason::Budget` and `axis: "iterations"` in the journal event at `iterations_used >= budget.max_iterations`. The outer-loop check sits before the dequeue, behind a side-effect-free `Queue::has_pending()` probe. A drained queue reports `NoWork` even at an exhausted budget. A pending unit the walk cannot afford reports `Budget`. It is never dequeued. A claiming `Queue`, like megawalk, acquires the node claim in `next()`. The probe does not acquire. So an unaffordable unit never has its claim taken. Parking holds the claim, so a park-shaped close does not release it.

**Resume guard (AC1-FR):** a pre-existing `termination` event for the manifest's `session_key` on the first `next()` call closes the unit without dispatch. This handles the case where the loop process was killed after the session completed but before the walk recorded the close. The close pass consumes one iteration. A budget counts work, not dispatches. A queue that re-derives the same unit must not spin for free. No duplicate dispatch occurs.

**Cancel:** the cancel closure checks `SIGINT_RECEIVED` (atomic bool set by a signal handler) OR the existence of `.fno/.target-cancelled`. Either trips `Interrupted`.

## The board-empty walk (king driver)

A target driver asks whether its one deliverable shipped. A king has no PR, so pointing the target driver at one can never reach a clean terminal state. `done_probes` are additive only. A plan can add conjuncts and can never silence the PR, CI, and review conjuncts underneath. The run burns to `NoProgress` or `Budget` while looking like it is working. The king driver asks the king's question instead, which is whether the board is clean.

`fno inbox board --json` reads seven queues through verbs that already exist and computes nothing they already answer. Every queue carries the literal shell command that produced it, so board emptiness is reproducible by a third party rather than asserted. The `undispatched` queue has an independent source. `fno backlog undispatched --json` scans graph entries and the complete node-claim snapshot. It does not reuse the ranked dispatch selector, so a selector omission is nameable rather than an empty queue.

| Queue | Read | King can shrink it |
|---|---|---|
| `operator_lane` | `cat ~/.fno/my-priorities.md` | yes, by filing a node or parking with a reason |
| `undispatched` | `fno backlog undispatched --json` | yes, by spawning a worker |
| `stalled_holder` | `fno agents claim list -J` + `fno backlog get <id>` + `fno agents peek <holder>` | yes, by one wake per node |
| `mergeable_pr` | `gh pr list` | only under `config.king.autonomous_merge` |
| `stale_claim` | `fno agents claim list -J --include-stale` | yes, by `fno agents claim reap` |
| `operator_question` | `fno inbox outstanding --json` | no, a human answers it |
| `unreachable_worker` | `fno agents needs --json` | not yet |

The split in that last column is what makes the loop converge. A queue only a human can clear holds the loop open forever, so it is reported and never counted toward done. The rule survives a broken verb. An unreadable report-only queue is loud (the board exits non-zero) and still uncounted. A human-gated queue never gates `NoWork`, and a failed reader behind it does not weaken that.

**Staffed is an activity reading, never a status word.** The roster's wire vocabulary collapses four real states into three words. A worker consuming tokens, a worker parked at its prompt, and a worker whose model refused all render alike. A board reading that word drops every stalled node out of its queue. It then terminates `NoWork` while claims are held and nothing moves, which is worse than no loop. The discriminator is membership in `_ACTIVE_STATES` plus a fresh activity age. The set is imported rather than copied, so a later vocabulary fix reaches the board without an edit here.

**Progress is a positive marker, never board size.** A king's board refills while it works. Dispatching a worker and merging a PR both create future work, so the actionable count can rise on the very fire that clears a row. Progress is therefore a row IDENTITY present on the previous fire and absent now, recorded as `actionable_ids` on each `king_loop_check`. That is external truth, read back off the board, and it needs no producer. A `king_action` naming an unseen `target_id` also counts, so a real producer works the day one lands, but nothing depends on one existing. Re-emitting the same `target_id` is not progress. That matters because a `stalled_holder` row can survive the only action a king has for it. The action is a wake, and it can fail, because the holder's model is the thing that refused. Three dry fires then terminate the loop `NoProgress` with a reason naming what stayed unshrunk. That is the truthful terminal for a stalled board.

**One arm, and a named hole where the second was.**

- *In-session:* `hooks/target-stop-hook.sh` adds `.fno/king-state.md` as a second candidate after its target search comes up empty and forwards `--driver king`. When both are present the target manifest wins: a session holding one is a worker whatever else sits beside it. A king terminal skips `finalize`, which stamps a plan and graduates a node that a king does not have. `hooks/agy-target-stop-hook.sh` cannot gate a king, because the loop is claude-only in v1. It names the manifest and refuses rather than allowing the stop. A guard on one of two reachable paths is decorative.
- *Cross-session:* there is none. A `KingQueue` walk arm was built and cut before it shipped. It had no working path. `run_loop`'s resume guard closed its unit without dispatching. The unit's `session_key` and the king's own termination event both used the manifest `fno_id`. It dispatched rarely, and dispatched the wrong thing. `CONTINUE_PROMPT` is hardcoded to `/target --resume` for every driver. `Unit.extra_env` is read by nothing. So the spawned session was a target resume with no idea it was a king. `fno-agents loop run --driver king` now refuses by name rather than falling through to the target walk.

**A `NoProgress` terminal asks the operator, exactly once.** `NoWork` is the clean exit. `NoProgress` is different. It is the king giving up with the board still full. That is this feature's own failure arriving through a different door. Work is pending, nothing moves, and nobody is told. So every `NoProgress` terminal calls `fno agents king escalate --stalled <rows>`. The verb records one question in the operator queue. `king_decide` reaches NoProgress two ways: an unreadable board, and a board nothing cleared. So the call sits in the shared `terminate` closure rather than at one site. That covers a terminal added later without anyone remembering to wire it, and a guard on one of several terminals is decorative.

The dedupe key is a hash of the sorted stalled row set. It is not the session and not the fire. A later king meeting the same stalled board must not file a second question. A board that CHANGED is a different ask and gets its own question. Rows are queue-qualified, as in `undispatched:<id>`. The same node in two queues is two rows, and clearing one of them is real progress. An unreadable board escalates over an empty set. The question text names that case, so the operator never has to tell "no rows" from "could not see". A failed escalation is named on stderr and folded into the termination message. It never blocks the terminal. The king stops either way, and a terminal that refused to finish because the ask failed is strictly worse.

**What a clean `NoWork` terminal means, and the edge it leaves open.** It means the king EXITS. When the board refills, NOTHING restarts it. This arm is a floor under an awake king and nothing more.

That edge is wider than a missing trigger. Nothing crowns a king either. The `king-for-a-day` skill never calls `fno agents king init`. When a king dies, nothing deletes the manifest. And a spawn can transfer a crown with no verb to return it. Crown, respawn and expire are one lifecycle and none of the three exists. A respawn arm bolted on before that lifecycle has no king to respawn, which is why the walk arm was cut rather than repaired.

Gated by `config.king.enabled` (default false), visible as the `king loop` row in `fno agents autonomy status`. It is the first row in that table that is a loop rather than a trigger. Every other one is woken by a PR merge, a launchd tick, a daemon tick, or a node's birth.

### `done_probes`: the operational-evidence conjunct

`DonePRGreen` normally conjuncts PR-exists + CI-green + review-clean + HEAD-shipped.
Every one of those measures an artifact, so operational silence cannot falsify the gate: a recurring deliverable can ship, pass CI, get reviewed, and never once run.
Grooming died this death three times.

A plan may therefore declare `done_probes` in its frontmatter - up to 3 shell commands that assert the thing actually ran:

```yaml
done_probes:
  - "fno agents mail list --kind report --since 24h | grep -q groom"
```

`loop-check` runs them as the **final** conjunct, only once every other conjunct already holds, and refuses `DonePRGreen` until each exits 0.
Ordering is what keeps the feature free: a plan with no declaration spawns no subprocess, and a red or unreviewed PR never pays for one either.

| Aspect | Behavior |
|---|---|
| Field absent, or `[]` | Zero probe subprocesses; gate behavior unchanged |
| Field present but unreadable (e.g. a multi-line inline list) | Refuse as "probes undeterminable". A declaration the parser cannot read is never treated as "no probes" - that is the vacuous pass the whole feature exists to prevent |
| More than 3 declared | Refuse, naming the cap; nothing executes |
| Non-zero exit (127 included) | Refuse, naming the verbatim command, the exit code, and up to 500 chars of stderr |
| Runs past 60s | The child's whole process GROUP is killed, and the result is a failure. The timeout is native Rust (spawn + `try_wait` + kill) because the host has no `timeout` binary |
| Plan unreadable, probes seen on a prior fire | Refuse as "probes undeterminable" |
| Plan unreadable, no probe history | Today's behavior - a probe-less session with a stale `plan_path` must not start refusing |

Two implementation constraints are load-bearing rather than defensive, and a future edit that "simplifies" either one reintroduces a hang.
A probe is typically a pipeline (`... | grep -q x`), so `sh` forks and killing `sh` alone would leave grandchildren holding the stderr pipe open - the drain thread would never see EOF and the join would block past the very timeout meant to bound it.
Hence the process-group kill, and hence stderr being drained by a reader thread at all (reading a piped stderr only after exit deadlocks any probe that writes past the pipe buffer).

A refusal where probes were declared but none actually ran (unparseable, over cap, plan unreadable) records `{"_undeterminable": "<cause>"}` rather than a bare `null`.
That matters because the fail-closed path keys off "did a prior fire record probes": a `null` would make the refusal invisible to it, so a plan that tripped the cap and then went missing would silently degrade to "no gate".

**Scope: `DonePRGreen` only.**
`DoneAdvisory`, `DonePlanned`, and `DoneBatched` return earlier and do not consult probes.
Those units ship no PR of their own, so there is no ship gate to hang evidence on; a recurring deliverable reaches its finish line through `DonePRGreen`.

Every fire records its results in the `loop_check` event as `data.done_probes` (`{"<cmd>": "pass" \| "fail:<code>" \| "timeout"}`). `fno whoami scoreboard --plan-fidelity` joins that against the declaration to report probes declared vs passed.

### Review coverage: `DoneUnreviewed`

The three `DonePRGreen` conjuncts (PR-exists, CI-green, review-clean, HEAD-shipped) all ask "did anyone object"; none asks "did anyone review."
A quota-refusing bot is dropped from the missing set and reads as a pass; on a config with no required bot, nothing can object, so `DonePRGreen` fired on zero reviews.
Review coverage is the missing predicate, computed as a first-class value and never folded back into the objection boolean.

Coverage counts `reviewed` verdicts across two **producer axes** (named by axis, not by string - the `chatgpt-codex-connector` GitHub App and the local `codex` CLI share a display name and are distinct):
`github_app` (review objects via the reviews API; can refuse on quota) and `local_attestation` (head-pinned `pass` `review_attestation` events; never quota-bound - `/code-review`, the codex CLI, sigma).
The local axis is presence-based: a head-pinned pass counts whether or not the reviewer is in `config.review.reviewers`, so a worker-run `/code-review` counts even when `reviewers: []`.
A head-pinned local pass makes coverage known even when the GitHub read failed, so a bot quota outage cannot wedge the autonomous path while a local lane reviewed - that is the PR #214 failure escaped rather than relocated.

A passing PR with coverage 0 or Unknown terminates `DoneUnreviewed` instead of `DonePRGreen`.
`DoneUnreviewed` is shaped like `DoneAwaitingMerge`: terminal on the first evaluation (no loop iteration spent waiting - that is what keeps the PR #214 wedge from returning), never a ship reason (out of `finalize.SHIP_REASONS`), never arms auto-merge.
The autonomous merge is therefore refused structurally (`should_arm_auto_merge = approved && reason == "DonePRGreen"`); a human or out-of-band merge closes the node via reconcile.
The discriminator is coverage, **not** the `attended` manifest field: `attended` is a known-broken substrate proxy (`FNO_AGENT_SELF` is injected by every spawn substrate including the pane default, so a spawned worker stamps `attended: false`), and the coverage path must not read it.

Coverage is reported everywhere from one source: loop-check computes it (the `review_coverage` event) and the Python readers consume that event rather than recomputing, so a human and the loop see one number.
The reachable merge paths it governs:

| Path | Coverage gate |
|---|---|
| Target spine (loop-check terminal) | coverage 0/Unknown -> `DoneUnreviewed`, never arms auto-merge |
| `fno do pr merge` (direct CLI) | reads `review_coverage`; zero/unknown/stale refuses (only when `auto_merge.enabled`) |
| reconcile (telemetry) | a zero-coverage out-of-band merge emits `gate_escape{zero-coverage}` even with no bots configured |
| `fno do pr status` | reports the `review_coverage` field (advisory) |
| `gh pr merge`, the web button, raw REST, the auto-merge queue | the server refuses an uncovered head once the merge ruleset is applied; the verdict is published as the `fno/review-coverage` status from the same source |

The verdict is published where GitHub can see it as two contexts on the PR head. `fno/review-coverage` is the required merge predicate. `fno/review-coverage-unavailable` is diagnostic instrument health and is never required by the ruleset. An unknown read sets both contexts to pending and tells the operator to retry the review verb. A computed covered or uncovered result preserves the required success/failure verdict and clears the diagnostic context to success.

`review-coverage-gate.yml` refreshes the status on the events only GitHub sees. A push invalidates the head. The `coverage-override` label arms or withdraws the release valve, naming its actor.

`apply-merge-ruleset.sh` makes only `fno/review-coverage` and `stacked-base-guard` required on the default branch. The bypass list is empty, so every client path is refused by GitHub itself rather than by advice. The diagnostic context remains outside the required ruleset so an instrument outage cannot be rendered as a code verdict.

One source also has to mean one *location*.

Every worktree resolves its local journal from the worktree root, and setup symlinks that file to the canonical checkout's journal. An isolated reviewer, author, merge, and reconcile all observe the same repository evidence.

`review_coverage` still goes to **both** the project and global logs with the full `host/owner/repo` identity. Cross-project readers cannot confuse repositories sharing a final path segment.

The project symlink removes the former cross-worktree visibility dependency. The global `~/.fno/events.jsonl` stays a cross-project mirror, not the mechanism that makes sibling worktrees agree.

Two sibling worktrees on the same exact HEAD share attestations by design. Session identity records whether the coverage came from the author or another reviewer.
`finalize.rs` reads the project log alone (`coverage_satisfied_in_latest_event`, `covered_head_from_event`) because finalize is invoked by the loop-check that just wrote the event, in that same directory; if finalize ever gains a caller that can stand elsewhere, those two become the same bug.

There is no environment override.
A probe that cannot pass here is fixed by editing the plan, which is visible in git; a wedged one falls to the existing `NoProgress` and `Budget` backstops.
Authoring guidance (freshness over bare existence) lives in `skills/blueprint/SKILL.md`.

### The stacked-base guard

Coverage answers whether anyone reviewed the PR. It says nothing about whether merging it puts the code anywhere.
A stacked PR names another feature branch as its base, and when that base lands on main without the stacked PR being retargeted, GitHub merges it into the dead base, reports MERGED, and the commits never reach main.
Specimen: PR #800 merged into `feature/bg-crown` at 2026-08-10T19:42:32Z, an hour after PR #789 landed that same branch on main; its merge commit `9b665db4` is the tip of `origin/feature/bg-crown` and is not an ancestor of `origin/main`.
Why the base was never retargeted is unexplained, so the guard asserts nothing about GitHub's retarget behavior and reads observable facts instead.

`cli/src/fno/pr/_base_lineage.py` is the one predicate: for a base that is not the default branch, it refuses if a MERGED PR already carried that base, or if the base tip is already an ancestor of the default branch.
Both checks are kept because they go blind in opposite directions - under `merge_strategy = "squash"` a landed base is not an ancestor of main, and a base that landed leaving no merged PR is invisible to the first check.
A third check covers where both go blind at once: `delete_branch_on_merge` removes the base as it lands, and a confirmed deletion is the verdict on its own, because a branch that does not exist carries nothing onward.
It has to be, since a fresh clone (every CI runner) has no `origin/<base>` to compare a tip against or resolve ancestry from, and a stale one is no better - under squash the landed base is no ancestor of the default branch, and a local ref left behind the merged head fails the first check's equality test too.
It fires only once `git ls-remote` answers, so a dead network still reads as `unknown` rather than forging a refusal.
It tests for a MERGED PR rather than requiring an OPEN one on purpose: stacking onto a base whose PR nobody has opened yet is legitimate work, and a guard that refuses it gets switched off.

Coverage of the eight reachable merge paths, stated honestly rather than implied:

| Merge path | Stacked-base guard |
|---|---|
| `fno do pr merge` (`_merge.py`), and its worktree API fallback | checked, refuses with `outcome=blocked` |
| `fno do pr verify --kind merged` bounded remediation (`_verify.py`) | checked, refuses the remediation |
| `finalize.rs` autonomous `--auto` arm | checked, refuses to arm |
| a PR update, via `.github/workflows/stacked-base-guard.yml` | checked for same-repo PRs; blocks only once marked a required status check. Fork PRs are skipped: their `GITHUB_TOKEN` cannot post the status |
| GitHub's `--auto` queue firing later, server-side | covered by the workflow's push-to-`main` sweep, which re-stamps the same status context |
| an agent-run bare `gh pr merge`, via `hooks/git-protection.py` | checked, denies before the two-factor path so the merge-gate override cannot buy past it |
| the GitHub web / mobile merge button | checked once the ruleset is applied: both contexts are required, so the button refuses an uncovered head |
| a human's `gh pr merge` in a plain terminal, or an unwired harness | checked once the ruleset is applied: the required `fno/review-coverage` context refuses an uncovered head |

The hook was nearly left unwired, on the argument that a guard over one of the ways a human runs a command invites the belief that the command is guarded.
That reasoning assumed a single harness and a mostly-human population; it is wired on both `hooks/hooks.json` and `hooks/codex-hooks.json`, and most merges here are agents running gh through a tool call.
It also already gated `gh pr merge` with its own two-factor check, so omitting lineage would have made that gate the incomplete one.

The residual hole was a human typing gh in a terminal. The required status context closes it, and the ruleset now commits that requirement as data.

The repo commits the ruleset as data: `scripts/ci/merge-ruleset.json` plus `scripts/ci/apply-merge-ruleset.sh`. The applier makes `stacked-base-guard` and only `fno/review-coverage` required on the default branch. The bypass list is empty, and the applier refuses to apply a file where it is not.

Applying it is an operator step: run `--apply` once, after a PR proves a green status on its own head.
One precondition before taking it: a `pull_request` event from a fork gets a read-only `GITHUB_TOKEN` regardless of the workflow's `permissions:` block, so the status POST fails and the context is never created for that PR.
The `guard` job therefore skips fork PRs outright rather than running and failing on the POST, which would have hung a permanently red check on every external contribution.
Marking it required while fork PRs are accepted blocks every one of them permanently, waiting on a context no run can produce, so the setting is safe only on a repo that takes no fork PRs; covering forks needs a privileged second workflow, which is a security decision this PR does not make.

The in-process callers fail OPEN on a probe that could not evaluate (a gh outage must not wedge a merge, matching `_merge._behind_by`), while CI fails CLOSED on the same condition, because a check that could not run has verified nothing.

`FNO_PR_BASE_LINEAGE_OK=stale-acknowledged` bypasses a refusal and records a `gate_escape`.

---

## The exec shim (`scripts/run-target-loop.sh`)

The 466-line bash loop body is replaced by a 74-line exec shim. The shim maps documented legacy flags onto `fno-agents loop run --driver target` and execs the binary. No loop logic lives here.

### Flag table

| Legacy shim flag | Maps to |
|---|---|
| `--driver <name>` | `--dispatcher <name>` (--driver target is pinned) |
| `--max-iterations` / `--max-iter` | `--max-iterations` |
| `--cli <alias>` | `--cli <alias>` (passed through) |
| `--max-turns N` | `--max-turns N` (passed through) |
| `--budget N` | `--budget N` (passed through) |
| `--model NAME` | `--model NAME` (passed through) |
| `--prompt-file PATH` | `--prompt-file PATH` (passed through) |
| unknown flag | loud rejection with migration message; exit 2 |

Unknown flags are rejected loudly with the message: `"The bash loop moved to 'fno-agents loop run' (step 5); this shim maps only the documented legacy flags."` No silent drops.

### Binary resolution order

Identical to `hooks/target-stop-hook.sh` (grilled decision 8):

1. `$FNO_AGENTS_BIN` (if set and executable)
2. `<repo>/crates/fno-agents/target/release/fno-agents`
3. `<repo>/crates/fno-agents/target/debug/fno-agents`
4. `command -v fno-agents` (PATH fallback)

Binary missing: the shim exits 2 with instructions to build or set `$FNO_AGENTS_BIN`.

The Rust verb (`run_loop_verb_inner`) also resolves `--cli` for the dispatcher's binary check. Precedence for the driver CLI binary (mirrors `driver-claude-code.sh`): `$CLAUDE_CLI` env > `--cli` flag > `$CLI` env > `"claude"` default.

### Exit-code map

| Code | Meaning |
|---|---|
| 0 | `DonePRGreen`, `DoneAdvisory`, `DoneDelivery`, or `NoWork` (unit terminated successfully) |
| 1 | `Budget`, `NoProgress`, or `Aborted` (walk failed or hit ceiling) |
| 2 | Usage error or internal error |
| 77 | Driver binary missing from PATH (preflight failure) |
| 130 | `Interrupted` (SIGINT convention) |

---

## Preflight

All checks run before any dispatch. A failed preflight never starts the walk.

| Check | Error path |
|---|---|
| Manifest exists (`.fno/target-state.md`) | exit 1, "run /target first to initialize" |
| `--driver` is in whitelist (`claude-code`, `hermes`, `openclaw`; megawalk uses its own verb) | exit 2, whitelist names stated |
| Driver lib file exists (`scripts/lib/driver-<name>.sh`) | exit 2, path stated |
| Lib defines `driver_invoke` (bash probe) | exit 2, "driver_invoke missing" |
| Driver binary is on PATH | exit 77, binary name stated |

The `--cli` alias is threaded through the binary check so preflight validates the same binary the dispatcher will actually use (not the process-global `$CLI` env, which could differ in tests).

`--max-iterations` defaults to the value returned by `driver_default_max()` - a bash shellout to `source driver-<name>.sh && driver_default_max`. Pass `--max-iterations` explicitly to override.

---

## What died

The legacy `scripts/run-target-loop.sh` was 466 lines. The code it contained is now either Rust or gone.

| Bash construct | What replaced it |
|---|---|
| `<promise>` grep in session output | typed `termination` event from `loop-check` verb; loop never reads stdout |
| Model-fallback chain (`sed`/`grep` state machine over model names) | intentionally NOT ported; loop-check budget/backstop + driver-level retries cover the failure modes; strangler flag parity only. The rate-limit/model-fallback class is deliberately not ported as an output-grep detector (locked decision: retry and backoff policies belong over typed events, not detectors); retry/backoff policy is the per-unit dispatch cap and consecutive-failure pause in the megawalk walk policy (group 2). |
| Restart-signal file polling | cancel closure checks `SIGINT_RECEIVED` + `.target-cancelled` sentinel |
| Multi-plan grep over session output | plan identity in manifest (`plan_path`), not parsed from stdout |
| Phase re-read / phase tracking | deleted by step-1 wedge; no phase state remains |
| Fingerprint / consecutive-fire counting | inside `fno-agents loop-check` (already Rust since the wedge) |
| Binary resolution (4 lines of if/then) | `resolve_driver_binary()` + `which_binary()` in `loop_dispatch.rs` |

Model-fallback is a deliberate drop, not an oversight. The loop contract is typed events; the budget backstop and driver-level retry (re-dispatch on `node_failed`) handle the failure class the fallback chain was attempting to manage. Porting the sed/grep model-name parser into Rust would resurrect complexity the step-5 design explicitly removes.

---

## The megawalk driver (group 2)

### MegawalkQueue

`MegawalkQueue` is the backlog `Queue` adapter. It never reads `graph.json` directly (grilled decision 7). All selection logic - epics-first ordering, project scoping, rank, `make_selection_sort_key` - lives inside `fno backlog next`. The queue shells two commands:

- **`next()`**: shells `fno backlog next [--project P | --all]` and parses the JSON response. A literal `null` output means the backlog is drained; `next()` returns `Ok(None)` and the walk terminates with `NoWork`. Malformed JSON or a non-zero exit is a `LoopError::Queue`.

- **`close()`**: shells `fno backlog done <id>` for `DonePRGreen | DoneAdvisory | DoneDelivery` evidence. Exit 0 yields `CloseOutcome::Closed`; exit 5 (PR OPEN, not merged) yields `CloseOutcome::AwaitingMerge`; other nonzero yields `CloseOutcome::Parked(stderr)`. `DoneAwaitingMerge` evidence maps directly to `CloseOutcome::AwaitingMerge` WITHOUT shelling `fno backlog done` (the reason already carries the fact - this fixes its earlier mis-handling as a held-claim Park). Other non-done evidence returns `CloseOutcome::Parked` without calling `fno backlog done`.

**Claims.** Before returning a unit from `next()`, the queue calls `fno agents claim acquire node:<id> --holder target-session:<session_key> --ttl 2h`. Exit 0 records the claim and returns the unit. Exit 1, `ClaimHeldByOther`, lets the live-claims filter inside `fno backlog next` exclude the node on the next retry. The walker needs no skip-set. Claims and selection compose without walker-side coordination. Exit 2, or any other non-zero code, surfaces immediately as a `LoopError::Queue`. Sigma-review finding 1: the previous collapse of all non-zero exits to "retry" hid validation and corruption errors. The retry bound is `MAX_CLAIM_RETRIES = 5`. Exhaustion is a `LoopError::Queue`. `has_pending()` answers from the same live selection without acquiring. The outer budget check probes it before any dequeue. A unit the walk cannot afford never has its claim taken.

**Park-exclusion.** On `CloseOutcome::Parked` or `Refused`, the claim is held (not released). The live-claims filter continues to exclude the parked node so the walker moves on to other ready work rather than re-picking the same stuck node. The claim TTL is refreshed via a same-holder re-acquire immediately after parking - the worker's `init-target-state.sh` rewrites `acquired_at` with the worker's (now-dead) pid, so the walker re-acquires to reset the window from the current time.

**`--max-units` (once mode).** When `max_units` is `Some(N)`, `next()` returns `None` after `N` units have been closed. Any close outcome - Closed, Parked, or Refused - counts toward the cap, so a walk of permanently-parking nodes cannot loop unboundedly. This maps the `/megawalk once` modifier (`--max-units 1`).

### TARGET_SESSION_ID correlation contract

The walker pre-generates a `session_key` in `gen_session_key()` (shape: `{utc}-mw{pid}-{6hex}`; the `mw` infix distinguishes megawalk-assigned keys from target-assigned keys in logs). `MegawalkDispatcher` injects two env vars before each dispatch:

- `TARGET_SESSION_ID=<session_key>` - consumed by `init-target-state.sh`, which uses the preset value verbatim rather than generating its own.
- `CONTINUE_PROMPT="/target --no-merge <unit.id>"`. When `--allow-merge` is set, `/target <unit.id>` instead.

Three consequences flow from this:

1. The `termination` event emitted by the worker's stop hook carries `session_id = session_key`, which `Journal::find_termination` matches against `unit.session_key`.

   Cross-worktree delivery works because each worktree journal symlinks to the canonical repository journal. The global `~/.fno/events.jsonl` mirror remains a fallback for cross-project observation.

   `find_termination` scans the project journal first, then falls back to the global mirror.

2. The worker's `init-target-state.sh` calls `fno agents claim acquire node:<id> --holder target-session:<session_key>` with the same holder string the walker used. `core.py:acquire_claim` line 209 treats a same-holder re-acquire as idempotent - it refreshes `pid/host/acquired_at` without blocking, emitting `claim_idempotent_reacquired`.

3. The walker's `close()` releases the claim using the recorded `session_key`, matching the holder the worker registered.

### Walk policy

**Consecutive-failure pause (3).** A "failure" is any `close()` that is not success-shaped: neither `Closed` nor `AwaitingMerge`. An `AwaitingMerge` close (done exit 5, or `DoneAwaitingMerge` evidence) records SUCCESS even though its reason is not a done-reason, so a run of healthy ship-green-awaiting-merge closes never trips the pause. Three consecutive failures trigger a `LoopError::Queue("pause:consecutive_failures:...")` on the next `next()` call. A successful close resets the streak - and also clears any pending p0 failure flag (sigma-review finding 3: without this reset a recovery success after a p0 failure caused a spurious immediate pause on the next unit).

**p0 immediate pause.** When a unit with `priority == "p0"` (from the `fno backlog next` JSON) fails, the next `next()` call returns `pause:p0_failed:<unit-id>` immediately, bypassing the 3-failure streak.

**Per-unit dispatch cap (15).** `run_loop` is called with `per_unit_max_dispatches = Some(15)`. A session that crashes without emitting a `termination` event is re-dispatched on the next inner loop iteration; after 15 dispatches the runtime closes the unit with `NoProgress` evidence and the streak counter sees a failure.

**`--parallel-cap`.** The flag is accepted and passed through. Group 2 serializes regardless of cap value (`run_loop` is single-threaded; collision-conservative default, Claude's Discretion 3). When `cap > 1`, the walk prints one honest notice rather than silently dropping the flag.

**Walker singleton.** At startup, `run_inner` acquires `walker:<cwd>` with holder `megawalk-loop:<pid>` and TTL 24h (`fno agents claim acquire`). A live claim means another megawalk is active for this project; the new invocation exits 1. The claim is released on every exit path including fatal loop errors.

### Hardened close (`fno backlog done` gh cross-check)

`fno backlog done` performs a gh cross-check before marking a node complete. For nodes associated with a PR, MERGED is the ONLY closing evidence (graph done = merged). An OPEN PR - even with green CI - is no longer closing evidence: it exits 5 (awaiting merge, `CloseOutcome::AwaitingMerge`), the node stays `in_review`, and `reconcile` / merge-triggered `advance` close it at the actual merge. CI state is not consulted in the close decision. Exit 3 is a refusal for CLOSED-unmerged / UNKNOWN (`CloseOutcome::Parked`); exit 4 is a gh outage (`CloseOutcome::Parked`). `--force` requires `--reason` and journals `backlog_done_forced`. Advisory nodes (no PR refs) are unaffected by the cross-check.

### New loop-stream event kinds (group 2)

Two new event kinds join the loop stream. Schema in `events-schema.yaml` only; NOT in `KNOWN_EVENT_KINDS` or `events-v3.json` - see "Two-tier event model" above.

| Event | When | Key data fields |
|---|---|---|
| `walk_paused` | Walk policy triggered a pause | `policy` ("consecutive_failures" or "p0_failed"), `detail` (unit ids involved) |
| `node_closed` | Unit close recorded | `unit_id`, `session_id`, `reason`, `close` ("closed", "parked", "refused", or "awaiting-merge"), `detail` |

**Legacy event migration.** The Python walker emitted ~29 kinds into `megawalk-events.jsonl` (deleted in task 2.4). Representative mappings: `node_complete` -> `node_closed{close:closed}`, `walker_paused` -> `walk_paused`, `consecutive_failures_paused` -> `walk_paused{policy:consecutive_failures}`, `backlog_empty` -> `loop_terminated{reason:NoWork}`. The full prune ledger is the comment block in `loop_megawalk.rs`. `megawalk-events.jsonl` as a write target is dead; the prune ledger records each legacy kind's fate for auditors.

### Front door

`/megawalk` (the Claude Code skill) launches `fno-agents loop run --driver megawalk` in the background and streams `.fno/events.jsonl` to show progress. `fno megawalk watch` (`megawalk_tui`) renders the canonical journal at ~1Hz via a Rich TUI. There is no separate `megawalk-events.jsonl`; the single `events.jsonl` is the authoritative walk record.

---

## The megatron driver (removed)

The megatron fleet-orchestration driver (`loop_megatron.rs`, `cli/src/fno/megatron/`, the `/megatron` skill, and the `--driver megatron` arm) was removed in the cutlist. `-C` spawn-into-project plus auto-worktree now covers multi-repo work, and a multi-repo feature is modeled as one backlog node per project linked by `blocked_by`, each shipping its own PR. The unified loop now exposes two drivers: `target` and `megawalk`.

### Batch-queue deprecation (task 3.2)

The `/batch-queue` command surface was removed (it was deprecated in the step-5 collapse, then dropped in the OSS-launch cleanup). The backlog subsumes it: `fno backlog intake` + `rank`/`blocked_by` + `/megawalk` express "run these plans in order" with claims and gh-cross-checked closes that the batch queue never had. The exit-12 `fno loop` stub was removed in the same change (zero callers; the group-2 grep and this group's re-grep both confirmed).

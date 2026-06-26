# Unified Loop Runtime

**Epic:** step 5, control-plane collapse
**Group 1 shipped:** runtime + target driver + exec shim
**Group 2 shipped:** megawalk driver
**Group 3:** planned - see "What lands later" below
**Sibling doc:** [control-plane-loop.md](control-plane-loop.md) - the stop-hook decision verb INSIDE a session

## Scope

This document covers the driver loop AROUND sessions: `crates/fno-agents/src/loop_runtime.rs`, `loop_target.rs`, `loop_dispatch.rs`, and the `scripts/run-target-loop.sh` exec shim. For the stop-hook decision verb that runs INSIDE each session, see [control-plane-loop.md](control-plane-loop.md).

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
  unit = queue.next()   -> None: NoWork (terminate)
  resume guard: journal has termination for unit.session_key?
    yes -> close without dispatch (AC1-FR; no iteration consumed)
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
    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError>;
}
```

`&mut self` is deliberate (locked decision F8): the group-2 megawalk Queue carries cursor state and consecutive-failure counters that are inherently sequential. Using `&self` would require pointless `Mutex`-wrapping for single-threaded walk state. `run_loop` is always called from one thread.

`close` returns `CloseOutcome`: `Closed`, `Refused(String)`, or `Parked(String)`. The runtime journals none of these directly - it records `loop_terminated` at walk end; the Queue impl is responsible for any additional side-effects on close (e.g. `fno backlog done` in group 2).

**Target Queue (group 1):** degenerate - one unit read from `.fno/target-state.md`. `close()` is inert: the session's own stop hook already emitted the `termination` event; the manifest is immutable. As of step 6 the session's terminal side-effects (ledger session-record, and on a ship the plan stamp/graduate + handoff artifact) are written by `fno-agents finalize`, which the shim invokes at the terminal-allow boundary BEFORE the worker process exits. The outer loop only observes the `termination` event afterward, so `TargetQueue::close` stays inert exactly as designed - placing the writes in `close` would miss attended interactive `/target` runs, which have no outer loop at all.

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

**Project journal (`<cwd>/.fno/events.jsonl`) is authoritative.** A write failure there stops dispatch loudly (`LoopError::Journal`). An unobservable walk that continues spending compute is worse than stopping.

**Global mirror (`~/.fno/events.jsonl`) is best-effort.** A write failure is logged to stderr and never propagated. The project journal is the record; the global mirror is convenience for cross-project tooling.

The method is named `append` (not `emit` or `emit_fields`) deliberately - see "Two-tier event model" below.

---

## The typed-event contract

The loop NEVER parses session stdout. The session's terminal state is communicated entirely through the project journal: the stop hook (`fno-agents loop-check`) emits a `termination` event when it decides to allow the session to exit. `Journal::find_termination` scans for the last matching event keyed on `unit.session_key` (= `session_id` from the manifest).

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

**Watchdog synthesis:** if a dispatched session exits and `find_termination` finds no matching event, the runtime emits `node_failed` with the exit code (including `128+N` for signal deaths) and re-dispatches on the next inner iteration.

**Iteration ceiling -> Budget:** when `iterations_used >= budget.max_iterations`, the walk terminates with `TerminationReason::Budget` and `axis: "iterations"` in the journal event.

**Resume guard (AC1-FR):** on the first `next()` call, if `find_termination` finds a pre-existing `termination` event for the manifest's `session_key`, the unit is closed without dispatch. This handles the case where the loop process was killed after the session completed but before the walk recorded the close. No iteration is consumed; no duplicate dispatch occurs.

**Cancel:** the cancel closure checks `SIGINT_RECEIVED` (atomic bool set by a signal handler) OR the existence of `.fno/.target-cancelled`. Either trips `Interrupted`.

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
| 0 | `DonePRGreen`, `DoneAdvisory`, or `NoWork` (unit terminated successfully) |
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

- **`close()`**: shells `fno backlog done <id>` for `DonePRGreen | DoneAdvisory` evidence. Exit 0 yields `CloseOutcome::Closed`; nonzero yields `CloseOutcome::Parked(stderr)`. Non-done evidence (any other `TerminationReason`) returns `CloseOutcome::Parked` without calling `fno backlog done`.

**Claims.** Before returning a unit from `next()`, the queue calls `fno claim acquire node:<id> --holder target-session:<session_key> --ttl 2h`. Exit 0 records the claim and returns the unit. Exit 1 (`ClaimHeldByOther`) lets the live-claims filter inside `fno backlog next` exclude the node on the next retry; the walker never needs a skip-set - claims and selection compose without walker-side coordination. Exit 2 or other non-zero codes surface immediately as a `LoopError::Queue` (sigma-review finding 1: the previous collapse of all non-zero exits to "retry" hid validation and corruption errors). The retry bound is `MAX_CLAIM_RETRIES = 5`; exhaustion is a `LoopError::Queue`.

**Park-exclusion.** On `CloseOutcome::Parked` or `Refused`, the claim is held (not released). The live-claims filter continues to exclude the parked node so the walker moves on to other ready work rather than re-picking the same stuck node. The claim TTL is refreshed via a same-holder re-acquire immediately after parking - the worker's `init-target-state.sh` rewrites `acquired_at` with the worker's (now-dead) pid, so the walker re-acquires to reset the window from the current time.

**`--max-units` (once mode).** When `max_units` is `Some(N)`, `next()` returns `None` after `N` units have been closed. Any close outcome - Closed, Parked, or Refused - counts toward the cap, so a walk of permanently-parking nodes cannot loop unboundedly. This maps the `/megawalk once` modifier (`--max-units 1`).

### TARGET_SESSION_ID correlation contract

The walker pre-generates a `session_key` in `gen_session_key()` (shape: `{utc}-mw{pid}-{6hex}`; the `mw` infix distinguishes megawalk-assigned keys from target-assigned keys in logs). `MegawalkDispatcher` injects two env vars before each dispatch:

- `TARGET_SESSION_ID=<session_key>` - consumed by `init-target-state.sh`, which uses the preset value verbatim rather than generating its own.
- `CONTINUE_PROMPT="/target no-merge <unit.id>"` (or `/target <unit.id>` when `--allow-merge`).

Three consequences flow from this:

1. The `termination` event emitted by the worker's stop hook carries `session_id = session_key`, which `Journal::find_termination` matches against `unit.session_key`. Cross-cwd delivery works because workers run `/target` in their own conductor worktrees; their termination events land in the worktree's `events.jsonl` AND the global `~/.fno/events.jsonl` mirror (via `loop-check`'s `emit_to_both`). `find_termination` scans the project journal first, then falls back to the global mirror.

2. The worker's `init-target-state.sh` calls `fno claim acquire node:<id> --holder target-session:<session_key>` with the same holder string the walker used. `core.py:acquire_claim` line 209 treats a same-holder re-acquire as idempotent - it refreshes `pid/host/acquired_at` without blocking, emitting `claim_idempotent_reacquired`.

3. The walker's `close()` releases the claim using the recorded `session_key`, matching the holder the worker registered.

### Walk policy

**Consecutive-failure pause (3).** A "failure" is any `close()` with `evidence.reason` not in `{DonePRGreen, DoneAdvisory}`. Three consecutive failures trigger a `LoopError::Queue("pause:consecutive_failures:...")` on the next `next()` call. A successful close resets the streak - and also clears any pending p0 failure flag (sigma-review finding 3: without this reset a recovery success after a p0 failure caused a spurious immediate pause on the next unit).

**p0 immediate pause.** When a unit with `priority == "p0"` (from the `fno backlog next` JSON) fails, the next `next()` call returns `pause:p0_failed:<unit-id>` immediately, bypassing the 3-failure streak.

**Per-unit dispatch cap (15).** `run_loop` is called with `per_unit_max_dispatches = Some(15)`. A session that crashes without emitting a `termination` event is re-dispatched on the next inner loop iteration; after 15 dispatches the runtime closes the unit with `NoProgress` evidence and the streak counter sees a failure.

**`--parallel-cap`.** The flag is accepted and passed through. Group 2 serializes regardless of cap value (`run_loop` is single-threaded; collision-conservative default, Claude's Discretion 3). When `cap > 1`, the walk prints one honest notice rather than silently dropping the flag.

**Walker singleton.** At startup, `run_inner` acquires `walker:<cwd>` with holder `megawalk-loop:<pid>` and TTL 24h (`fno claim acquire`). A live claim means another megawalk is active for this project; the new invocation exits 1. The claim is released on every exit path including fatal loop errors.

### Hardened close (`fno backlog done` gh cross-check)

`fno backlog done` performs a gh cross-check before marking a node complete. For nodes associated with a PR: MERGED state allows the close; OPEN with green CI also allows. Exit 3 is a refusal (`CloseOutcome::Refused`); exit 4 is a gh outage (`CloseOutcome::Parked`). `--force` requires `--reason` and journals `backlog_done_forced`. Advisory nodes (no PR refs) are unaffected by the cross-check.

### New loop-stream event kinds (group 2)

Two new event kinds join the loop stream. Schema in `events-schema.yaml` only; NOT in `KNOWN_EVENT_KINDS` or `events-v3.json` - see "Two-tier event model" above.

| Event | When | Key data fields |
|---|---|---|
| `walk_paused` | Walk policy triggered a pause | `policy` ("consecutive_failures" or "p0_failed"), `detail` (unit ids involved) |
| `node_closed` | Unit close recorded | `unit_id`, `session_id`, `reason`, `close` ("closed" or "parked"), `detail` |

**Legacy event migration.** The Python walker emitted ~29 kinds into `megawalk-events.jsonl` (deleted in task 2.4). Representative mappings: `node_complete` -> `node_closed{close:closed}`, `walker_paused` -> `walk_paused`, `consecutive_failures_paused` -> `walk_paused{policy:consecutive_failures}`, `backlog_empty` -> `loop_terminated{reason:NoWork}`. The full prune ledger is the comment block in `loop_megawalk.rs`. `megawalk-events.jsonl` as a write target is dead; the prune ledger records each legacy kind's fate for auditors.

### Front door

`/megawalk` (the Claude Code skill) launches `fno-agents loop run --driver megawalk` in the background and streams `.fno/events.jsonl` to show progress. `fno megawalk watch` (`megawalk_tui`) renders the canonical journal at ~1Hz via a Rich TUI. There is no separate `megawalk-events.jsonl`; the single `events.jsonl` is the authoritative walk record.

---

## The megatron driver (removed)

The megatron fleet-orchestration driver (`loop_megatron.rs`, `cli/src/fno/megatron/`, the `/megatron` skill, and the `--driver megatron` arm) was removed in the cutlist (x-f539). `-P` spawn-into-project plus auto-worktree (x-9c4c) now covers multi-repo work, and a multi-repo feature is modeled as one backlog node per project linked by `blocked_by`, each shipping its own PR. The unified loop now exposes two drivers: `target` and `megawalk`.

### Batch-queue deprecation (task 3.2)

The `/batch-queue` command surface was removed (it was deprecated in the step-5 collapse, then dropped in the OSS-launch cleanup). The backlog subsumes it: `fno backlog intake` + `rank`/`blocked_by` + `/megawalk` express "run these plans in order" with claims and gh-cross-checked closes that the batch queue never had. The exit-12 `fno loop` stub was removed in the same change (zero callers; the group-2 grep and this group's re-grep both confirmed).

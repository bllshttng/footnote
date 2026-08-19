# Work-claim coordination

This document describes the `fno claim` primitive, its on-disk format, the
key namespace, and the relationship to the older coordination paths it
replaces.

## Problem

Before this work, three partially-overlapping coordination mechanisms ran
in parallel:

- **Graph node `session_id`** (in `~/.fno/graph.json`): set by
  `roadmap-tasks.py update --locked-by` to mark "this backlog node is
  being worked on by session X." Authoritative for `fno backlog next`
  filtering, but not visible to subsystems that did not load the graph.
- **Megawalk PID lock** (`pid` field in `megawalk-state.md`): a walker-singleton lock so two megawalk walkers in the same project could not both run. Used `os.kill(pid, 0)` for liveness; no PID-reuse detection. (Now replaced by the `walker:` claim.)
- **Megawalk `in_flight_nodes`** array: per-walker tracking of which graph nodes the walker had dispatched. Filtered out at `_select_ready_nodes` to avoid double-dispatch within the same walker. (Now replaced by the `node:` claim filter in `fno backlog next`.)

These three did not agree on a unified "what is in flight right now" view.
Cross-project work (megatron) had no equivalent layer at all, leading to a
parallel-worktree race where two operators could both invoke
`/target <node-id>` against the same node from different worktrees and both
target sessions would dispatch.

## Design

One primitive, flat key namespace, atomic file-based locking, append-only
audit trail.

### Key namespace

Keys are colon-separated strings with a typed prefix. URL-encoded on disk
so the prefix's colon is filename-safe.

| Prefix | Holder shape | Used by |
|--------|--------------|---------|
| `node:` | `target-session:<sid>` | target init / stop hook |
| `walker:` | `megawalk-loop:<pid>` | megawalk singleton |
| `fleet:` | `megatron-commander:<pid>` | megatron `run()` |
| `project:` | `megatron-project:<mission>:<project>` | (reserved) per-project worker |
| `user:` | (reserved) | (future) human-imposed locks |

Holder shape is convention, not enforced. The verb-level invariants are:

- Idempotent re-acquire iff `existing.holder == new.holder`.
- Stale recovery iff the existing holder is structurally dead
  (PID-liveness: process gone or PID reused; TTL: expires_at in the
  past).
- Otherwise raise `ClaimHeldByOther`.

### Liveness model

Two modes, mutually exclusive per claim:

- **PID-liveness** (default; `expires_at` omitted from the YAML). The
  holder process must be alive on `claim.host` AND its `create_time`
  must precede `claim.acquired_at`. The create_time check catches PID
  reuse (a long-running OS, a recycled init namespace).
- **TTL** (`expires_at` set to an epoch-ms). Refresh extends. Useful
  for operators or cron-driven processes where PID is meaningless.
  Valid range is 60s to 24h.

**Hybrid arm (TTL claims that also record a pid).** A TTL claim whose
clock has expired is not unconditionally stale: if its recorded pid is a
live process on this host (passing the same host + `create_time` guards as
PID-liveness), it stays LIVE. This keeps a session that is alive but idle
or SIGSTOP-suspended past its TTL from having its claim reclaimed by a peer,
a case TTL refresh cannot cover because a suspended process cannot run its
own refresh. The arm is purely additive: it only ever extends liveness, so
a TTL claim whose recorded pid is transient, dead, missing, or off-host
falls to STALE on expiry exactly as a plain TTL claim does. `node:<id>`
target claims opt in by recording a durable session pid (see below); the
megawalk walker records a transient pid, so the arm never fires for it and
its TTL park-exclusion is unchanged.

**Suspect state + skip-not-steal.** A TTL claim still *inside* its
window whose recorded pid is not live classifies as `suspect`, not `live`.
This is the respawned-worker case: a bg `/target` supervisor pid dies and the
claude session keeps working under a new pid, so the pid arm can no longer
*prove* liveness but the TTL still protects the slot. The governing principle
is that contested or ambiguous liveness degrades to **skip**, never to
**steal** and never to a stalled lane:

- **acquire** treats `suspect` exactly like `live` and refuses (never reclaims
  it). Only TTL expiry (`suspect` -> `stale`) makes a claim reclaimable; pid
  death alone never frees one.
- **dispatch** (`fno agents spawn-guard`, `dispatch-node.sh`, `backlog next`
  selection) skips a `suspect`-claimed node with a `skipped-contested` outcome
  and advances to the next unblocked ready node - it does not park the lane.
- **lease renewal** rides `fno-agents loop-check`: on every stop, if the
  manifest holder matches the lockfile holder it extends `expires_at` by the
  claim TTL, so a respawned worker keeps its claim alive under any pid with no
  separate heartbeat. Renewal is best-effort; a missed renewal only shortens
  the lease, never blocks the loop.
- **`fno target init`** may archive-and-reclaim a prior manifest only when the
  lockfile is `free`/`stale`/`corrupted` AND the worktree shows no fresh activity
  within `config.claims.activity_window` (default 15m, env override
  `TARGET_CLAIM_ACTIVITY_WINDOW`). Freshness is the newest mtime among
  git-tracked-modified files and the `.fno/scratchpad` tree - a live `/target`
  writes there continuously. (Deliberately mtime-only: init's own parent process
  is legitimately cwd'd in the worktree, so a "process cwd'd here" check would
  false-positive every run.) A `suspect` claim, or fresh activity, makes init
  refuse as `contested` (`RESULT: BLOCKED`) rather than steal.

The live lockfile holder is the only ownership truth: the `target_claim_*`
manifest fields are an init-time snapshot and graph `status: claimed` names no
holder, so all guidance compares `fno claim status` against the session's own
id, never a snapshot.

`is_live` returns False for cross-machine claims. The design explicitly does
not support multi-host coordination - operators running two hosts on the same
shared filesystem will see both claims as "opaque, not mine to release."

"Same machine" is decided by `claims/hostid.py` (`is_same_machine`), NOT by a
raw `gethostname()` compare. Identity scopes PID-reuse detection, so it has to
be as stable as the pid namespace it scopes, and `gethostname()` is not: on
macOS with `scutil --get HostName` unset it is derived from whatever DHCP/DNS
last supplied and flips on network join, VPN, and sleep/wake. A name that moved
mid-session made a live holder read as cross-host, which short-circuits
`is_live` before the pid check and drops the claim to `stale` - and `stale` is
recoverable, so the node became stealable out from under a working session.

Liveness therefore compares a `machine_id` field: IOPlatformUUID on macOS,
`/etc/machine-id` on Linux, `gethostname()` where neither is readable. On Linux
it also carries the `/proc/self/ns/pid` inode, because containers built from one
image share `/etc/machine-id` while holding independent pid namespaces; without
it, two such containers sharing a claims root would read each other's pids as
local and a dead foreign claim would classify live forever.

`machine_id` is ADDITIVE and `host` still holds `gethostname()`. Overwriting
`host` would break the reverse direction of a rolling upgrade: a still-running
pre-change reader compares `host` to its own `gethostname()`, would miss, and
would call a live claim stale - stealable, the same bug from the other side.
Absent `machine_id` (a pre-change claim) falls back to the host compare, so such
claims classify exactly as they always did. This mirrors how `harness` was
added. Both Rust mirrors (`claims.rs`, `agents_view.rs`) carry the same pair -
all three writers must agree or each reads the others' claims as cross-machine.

### Atomic write

`acquire_claim` uses `os.open(path, O_CREAT|O_EXCL|O_WRONLY)`. The kernel
guarantees that only one caller wins. Losers see `FileExistsError` and
either:

1. Read the existing file. If holder matches, idempotent re-acquire
   (rewrite with refreshed pid/host/acquired_at).
2. Classify the existing claim. If stale: enter the recovery mutex
   (a mkdir on `<path>.recovery.d`), archive the stale claim to
   `.expired/<encoded-key>.<ts>.lock`, and re-attempt the exclusive
   create.
3. Otherwise raise `ClaimHeldByOther`.

The mkdir-based recovery mutex closes the TOCTOU window where two
workers both observe a stale claim, both archive (the loser's archive
no-ops because the rename target is already gone), and both successfully
exclusive-create in the brief empty-path window between archive and
recreate.

### Audit trail

Seven event types route through the existing `events.jsonl` validator
(see `cli/src/fno/events/schema.yaml`):

| Event | When |
|-------|------|
| `claim_acquired` | First write for a key |
| `claim_released` | Holder unlinked the file |
| `claim_refreshed` | TTL extended |
| `claim_stale_reclaimed` | New holder took over a dead/expired claim |
| `claim_force_overridden` | Operator override via `fno claim release --force` |
| `claim_idempotent_reacquired` | Same holder re-acquired (resume) |
| `claim_clock_skew_rejected` | Refresh would set expires_at in the past |

Emit failure is best-effort: the YAML lock file is the authoritative
state; the events log is for observability and forensics.

## Contract with gates

Claims and gates are independent. Gates verify "this phase produced
its expected artifact + provenance event"; claims verify "this work
unit is owned by someone right now." Both are structural verification,
operating at different granularity. A session may flip a gate without
holding any claim (e.g. a one-shot tool) and a claim may be held by
a session whose gates are all false (it just started).

## Selection-time enforcement (node claims)

`node:<id>` claims are the cross-session mutex that stops two `/target`
sessions (or a `/target` racing a megawalk-dispatched target) from both
picking up the same backlog node. Two properties make this work, and both
differ from the per-walker `walker:` claim:

- **Global root.** Node ids are global (they live in `~/.fno/graph.json`),
  so the lock must coordinate across worktrees, not land in a worktree-local
  `cwd/.fno/claims`. Node-claim call sites set `FNO_CLAIMS_ROOT=$HOME`
  so the lock is written to `~/.fno/claims`, a sibling of the global
  graph. `claims_dir()` honors that env var when no explicit `root` is passed;
  `global_claims_root()` is the in-process resolver (env, else `$HOME`). The
  `walker:` singleton keeps its per-root (cwd) location by passing an explicit
  root, so it is unaffected.
- **TTL with a durable-pid hybrid arm.** A `/target` node claim is acquired by
  the one-shot `fno target init` subprocess, which exits immediately - so a pure
  PID-liveness claim would be stale on birth and the next session would reclaim
  it. Node claims are therefore TTL claims (`--ttl ${TARGET_CLAIM_TTL:-2h}`),
  acquired in `init-target-state.sh` and released by the stop hook. To stop a
  session that is alive but idle or suspended past its TTL from being reclaimed,
  init ALSO records a durable session pid (`--pid`, resolved by `fno claim
  session-pid` walking the process tree to the nearest `claude` ancestor) so the
  hybrid arm keeps the claim LIVE while that process lives. This is degrade-safe:
  if the durable pid is uncapturable, no `--pid` is recorded and the claim is
  TTL-only exactly as before. A crashed session's lock self-heals when its pid
  dies and the TTL expires.

Two enforcement points:

1. **Acquire/refuse at dispatch** (`init-target-state.sh`). The node id resolves
   from a bare `/target ab-XXXX` input or a plan path mapping to a graph entry.
   On `ClaimHeldByOther` (exit 1) the init touches `.fno/.target-cancelled`
   so the stop hook authors `BLOCKED`. A non-contention acquire error (transient,
   or an older `fno` that predates `--ttl`) does NOT block - the session proceeds
   without a claim rather than wedging during an upgrade window.
2. **Filter at selection** (`fno backlog next` / `backlog ready`). Candidates holding
   a live `node:<id>` claim are dropped before sorting, so the walker never hands
   out a node a live session already owns. Best-effort: a claims-subsystem fault
   degrades to no filtering (the acquire/refuse mutex above is the authoritative
   backstop).

## Operator runbook

**Who holds a claim?**

```bash
fno claim status node:ab-1234abcd      # one key
fno claim list --prefix node:          # all node-level claims
fno claim list --include-stale         # include dead holders
```

**A claim is stuck.** First, check whether the holder is genuinely dead:

```bash
fno claim status node:ab-stuck
# state: live  holder: target-session:s-abc  pid: 12345  host: workhost
ps -p 12345    # if "process not found", PID-liveness will reclaim on next acquire
```

If the holder PID is alive but the work is genuinely stuck (e.g., an
operator killed `/target` with SIGKILL and an orphan child remains),
force-release with an audit trail:

```bash
fno claim release node:ab-stuck --force --reason "operator intervention; SIGKILLed target 2026-05-19"
```

The archived claim survives in `.fno/claims/.expired/`.

**Why isn't megawalk picking up this ready node?** Cross-check the
graph status against any held claim:

```bash
fno backlog get ab-thisnode
fno claim status node:ab-thisnode
# If state=live but the graph node is "ready" rather than "in_flight",
# the walker may have lost its in-process state and is still trying
# to dispatch. Inspect the holder PID and walker state.
```

## Reference implementation

PID-liveness + `O_CREAT|O_EXCL` + idempotent re-acquire is a standard
file-locking pattern for single-host mutual exclusion. Tests in
`cli/tests/integration/test_claims_concurrency.py` exercise the race
shapes (concurrent acquire, stale recovery, and the TOCTOU window on
archive-then-recreate).

## Coordination today

`fno claim` is the coordination primitive across target, megawalk, and
megatron. Megawalk's legacy coordination mechanisms (`megawalk-state.md`,
`in_flight_nodes`, the PID lock) have been removed in favor of the
`walker:` and `node:` claims. `fno claim list` + `events.jsonl` provide
observability into what is in flight.

One legacy mirror remains: `/target` still writes a graph `session_id`
onto the backlog node when it claims (alongside acquiring the `node:`
claim), and that field independently derives the node's `claimed` status.
A stuck target can therefore leave a node marked claimed in
`fno backlog get` even when `fno claim list` is clean, so recovery should
check both the claim and the graph node status (as the runbook above does).

Deferred for separate plans:

- Per-project claims (`project:<mission>:<project>`) inside megatron's
  per-project dispatcher - the commander-level `fleet:` claim is the
  load-bearing race prevention.
- Cross-host claim coordination - intentionally out of scope.
- Web UI / TUI for claim inspection - `fno claim list` covers it.

## Agent primitives: citizens and limbs

fno models one agent-spawn primitive, `fno agents spawn`, but two run in
every session: the fno-spawned *citizen* and the harness-native *subagent*
(a *limb*).
The contract below names the difference so the choice rests on properties,
not folklore, and records why the limb cannot be made fully addressable.

### The two primitives

A **citizen** is what `fno agents spawn` produces: a row in the fno agents
registry, discoverable by `fno agents peek`, addressable by `fno mail`,
holding its own `node:` claim, surviving its spawner's death, and
handoff-able to a successor king.
Its transcript sits beside its peers at the project root as
`~/.claude/projects/<proj>/<session-id>.jsonl`.

A **limb** is a harness-native subagent - Claude's Agent tool, or the
task/agent primitive on codex, agy, and opencode.
It is a nested conversation inside its parent's session process: no pid, no
roster entry, no registry row, no mail handle, and no transcript of its own
at the project root (it lives at
`<proj>/<parent-session-id>/subagents/agent-<id>.jsonl`).
It reports to its spawner or to nobody.

### When to use which

Reach for the **limb** when the work is one-shot, you consume the result in
your own next turn, and nothing outside you needs to observe, message,
drive, or inherit it.
It is genuinely the better tool there: no spawn latency, no worktree setup,
no registry write, parallel fan-out inside one context, and the result lands
back in a context that already holds the problem.
This is plausibly why a measured subagent caught a config alias a design doc
had missed - it returned into the context that held the design rather than
into a registry row someone would have to go read.

Reach for **`fno agents spawn`** when any of these hold: the work must
outlive its spawner; someone other than the spawner must observe, message,
or drive it; it must be handed to a successor king; it holds a `node:`
claim, since the registry row is what makes the claim attributable; it must
join king-mediated review, which is mail-shaped and therefore needs a
handle; or it needs its own worktree or branch.

Neither primitive is always correct.
"Always spawn" discards the limb's real advantages; "always subagent"
orphans anything that must outlive its parent.

### Why full addressability is rejected

The honest ceiling for a limb is **read-only visibility**, not
addressability.
A subagent has no input stream of its own: `fno mail send` injects as
user-shaped text into a session's input, and a limb has no input to inject
into.
Inventing a second delivery path for subagent mail would be exactly the
"guard placed on one of N reachable paths" trap this repo has already paid
for, with the added cost that the new path would be the one nobody tests.
Addressability is therefore rejected by design, not deferred as an ambition
to revisit every quarter.

What is shipped instead is observation: `fno agents top --subagents`
enumerates sidechain transcripts (keyed on `agentId`, not pid, since a limb
has no pid), lists each with its parent session and an mtime-based liveness
verdict against a stated threshold, and is claude-only today.
The codex, agy, and opencode task primitives have their own on-disk layouts
that are unmeasured here; a future harness reader slots into the same
per-harness discovery seam.
The subagent source is a display-only third input to `fno agents top`; it is
deliberately not wired into `census()`'s slot arithmetic, so it cannot move
the spawn gate's `slot_count` denominator.

## Two axes: reachability and progress

`fno.agents.reachability` (`cli/src/fno/agents/reachability.py`) answers two different questions about one row, in two separate fields, on purpose.

**`reachability`** answers "can I reach this process": `reachable` | `unreachable` | `unknown`, with a `basis` naming the evidence (`transcript` | `process-gone` | `pane-gone` | `silent` | `no-evidence`). It is the older axis and `fno agents list --status` still filters on its rendered wire word (`live` | `orphaned` | `unknown`).

**`progress`** answers a question reachability cannot: "is it advancing, awaiting the operator, parked, or refused" - `advancing` | `awaiting-operator` | `parked` | `refused` | `unknown`, with a `progress_basis` naming the evidence (`transcript-turn` | `operator-turn` | `promise` | `model-refused` | `silent` | `no-evidence`). `advancing` requires a transcript advance inside the ten-minute evidence window; a self-written `working` state with older output is `unknown` / `silent`, and an unreadable activity age is `unknown` / `no-evidence`. `fno agents list --progress` filters it independently of `--status`.

They stay two fields rather than a fourth `WIRE_STATUS` value because a worker taking its own next turn, one that finished and parked, and one that is alive and reachable but handed a model its endpoint cannot serve all classify `reachable` - correctly, since all three genuinely are reachable. Before this axis existed, all three also rendered the SAME word, `live`, so an operator scanning `fno agents top` could not tell a staffed row from a stalled one: 31 live pids, 8513 MB, and no way to see which rows were parked or refused.

A reading about one artifact is not a verdict about the agent. A missing transcript file proves only that a file is missing at that path; it must classify `unknown` on both axes, never a death verdict and never `refused` or `parked`. Every falsifier in `reachability.py` (`pid_falsifier`, `pane_falsifier`) already returns `None` - not a condemnation - when its own evidence is absent or unreadable, and `classify_progress` follows the same rule for the transcript's model reading. Only the classifiers in `reachability.py` may answer either axis; a reader that derives liveness or progress from bare file existence anywhere else is rebuilding the collapse this module exists to prevent, one layer over.

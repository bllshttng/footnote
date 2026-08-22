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
- **`fno do target init`** may archive-and-reclaim a prior manifest only when the
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

**A held claim proves a holder, never a worker.** A hand `fno claim acquire` takes `node:<id>` with nothing launched. A `spawn-handover:` claim covers a launch window whose worker can die before it boots. So the guard reports three reasons, not two. `live-claim` means a holder AND a `fno target init` behind it. `unproven-claim` means a holder and nothing more. That is what `spawn.sh` and `dispatch-node.sh` render as "no worker has reached target init" instead of the old "live worker holds node". `suspect-claim` is unchanged. The discriminator is `_init_reached` in `cli/src/fno/agents/cli.py`. It reads two markers. The `target-session:` holder prefix is a convention, not proof, since a hand acquire writes the same string. The manifest under the dispatcher's cwd must bind `target_claim_key: node:<id>` AND name the observed holder. That holder match is the non-forgeable half. A read fault answers unproven, because an unreadable manifest must never manufacture a worker. `fno backlog next --claim H --external` needs two things to protect the node it hands out, and it had neither. It now routes through `claims_root_for`, because a `node:` lock in the cwd-default tree reads free to every dispatch surface. It also carries `EXTERNAL_SELECTION_TTL`, because selection exits as soon as it prints the node. A pid-liveness claim from a process that exits reads `stale`, and `stale` does not block a dispatch.

**The reservation is taken after the launch is proven.** `cmd_spawn` runs its node guard below the resume-provider resolution. An exit on that stretch now strands neither `dispatch:<id>` nor the handover `node:<id>`. Below it the only exit is `run_gate`, whose `except BaseException` releases both. On the way out, the reservation is released whenever the substrate is a one-shot (`--once` or `--substrate headless`), as well as on a failed spawn. When the call returns, a one-shot's worker has already exited, so nobody is left to inherit it. `pane` and `bg` keep it, because their worker outlives the caller.

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
  the one-shot `fno do target init` subprocess, which exits immediately - so a pure
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

## Who writes `node:<id>`, and who can prove it dead

Measured 2026-08-19: nine nodes each named by a live roster worker, and seven read `free`. Two live claimants landed on one node and a third nearly did. The claim is documented as THE work-claim primitive, so four kings read `free` and staffed duplicates onto nodes that already had someone on them.

The cause was a guard on one of many paths. `node:<id>` had exactly one producer, a shell script that tokenized a prompt string, and it only ran under `fno do target init`. Every other route to a node bypassed it: a verb buried mid-sentence, a payload naming two ids, a run that went through `/blueprint` instead. This section records the rules that replaced it, because each one is a trap somebody will otherwise re-derive.

### Exactly two producers, and that ceiling is load-bearing

`fno do target init` and `fno agents spawn --node`. Those two callers hold the node id as a TYPED argument, not as prose to be re-derived. Adding a third is a design change, not a fix. Some paths have no node id at all, such as a prose payload or a hand-started session. Adding producers leaves those permanently unclaimed. That is the same decorative-guard shape wearing a new coat.

`/blueprint` deliberately does NOT claim. It writes a plan and does not build. A claim taken there is held across the plan-then-target gap and blocks the target run that follows it. Its frontmatter `claims:` field is plan adoption, an unrelated concept that shares the word.

The ceiling is also what makes the abandonment probe below safe. That probe assumes a node claim always has a roster-visible counterpart, and both producers create one. A third producer that claims a node without writing a worktree manifest or a registry row makes the probe report a live worker as abandoned.

### The reader is the half that covers every path

No producer can reach a prose payload or a hand-started session, so `fno claim status node:<id>` cross-checks the fleet roster before it renders a `free` reading. It names its instrument in four distinct ways, and each string is produced by exactly one outcome:

```
UNCLAIMED but a live worker is on this node: <rows>
free, no live worker found (roster scanned: N rows)
free, no live worker found (...); M finished session(s) resolved to it: ...
free, no row resolved to this node (N scanned, M unresolved); ... Confirm with: fno agents peek <name>
free, roster not consulted (<reason>)
```

The scanned count is the point. A scan of forty rows finding nobody is a different answer from a read that failed. Printing `free` for both is how the defect survives its own fix. Assert one of these strings. Never grep for the absence of the word `free`.

`roster_rows_unresolved` is the count of scanned rows whose worktree manifest or ledger did not resolve a node. If a worktree basename matches, the reader reports a candidate with `fno agents peek <name>`. It never acquires or infers a claim. `state: free` remains the claim answer.

The join resolves a row's node from the worktree manifest and then the session-keyed ledger, both machine-written. **Never a name regex.** Eight auto-named workers read as nobody-on-this-node on 2026-08-15 and were nearly double-dispatched. Worker names carry their node only by convention, and a convention is not a guard.

### Two kinds of death, two proofs

The store also failed in the opposite direction. When nobody was building, it said HELD. Which proof a claim needs depends on what its holder is.

| Claim | Holder | Can it come back under a new pid? | Proof of death |
|---|---|---|---|
| `node:<id>` | a session | yes, the daemon respawns it | pid dead AND the holder's own roster row found, reading terminal |
| `dispatch:<id>` | `spawn-cli:<pid>` | no, but its TTL is the boot window | expiry in a sweep, dead pid at the guard about to launch |
| any, expired TTL | any | n/a | the clock, from any host |

An **expired TTL** is a wall-clock fact and needs no same-machine proof. Requiring one is what made the store fill forever. This machine wrote `BB16s-MBP`, `BB16s-MacBook-Pro.local` and a tailnet name within one hour. Rows predating the `machine_id` field carry only that moving name, so nothing satisfies their same-machine proof.

**`machine_id` is authoritative whenever present** and every new same-machine proof keys on it through `is_same_machine`, never on a hostname compare. The hostname fallback exists only for pre-`machine_id` rows, and reproducing it exactly is deliberate: those classify no worse than they did before the field existed.

A **`node:` claim is reaped only on a positive finding, and that finding is the HOLDER**. The probe resolves the claim's own holder session id. It requires that row to exist in the roster and to read terminal. Everything else answers unknown, and **unknown KEEPS the claim**.

The asymmetry is the whole safety argument. An earlier version asked whether any row resolved to the node, read the empty answer as abandonment, and defended it with a scanned-row count. A row count validates the INSTRUMENT, never the TARGET. `fleet_rows` enumerates `claude agents --json --all` and drops interactive rows. A codex worker, an opencode worker, and any hand-started session are invisible to it by construction. A forty-row scan that cannot represent the holder reads as forty rows of proof.

The roster's coverage gap now costs a missed reap instead of a wrongly archived claim. That is the correct direction to fail. An unreaped claim expires on its own, and a wrongly reaped one hands a live worker's node to a second worker.

A `dispatch:` reservation looks like it never needed SUSPECT's protection, since nothing respawns under `spawn-cli:<pid>`. But its TTL is the BOOT WINDOW. It outlives its spawner on purpose, so a second dispatcher does not launch onto a node whose worker has not reached `fno do target init`. A sweep must not reap it, and only expiry frees it there. The spawn guard clears it instead, re-probing its own reservation key at the moment it is about to launch. That is safe because the node claim now covers the same window.

The one exception is a **launch window**. A claim held under `spawn-handover:<worker>` is exempt from the probe. Between the spawn and the worker's first `fno do target init` there is no worktree manifest and no ledger row. So the roster cannot see that worker BY CONSTRUCTION. Nothing is stranded by the exemption: that claim is TTL-bound, and an expired claim is provably dead on its own.

### Node closure releases the claim and the reaper settles node-aware

Measured 2026-08-21: a live session finished one node and moved to the next. Its claim on the done node read LIVE for 16 hours, past its own TTL. The reaper only asked whether the holder was alive. Two fixes, both at single choke points.

**Closure releases.** Every Python closure path persists through `locked_mutate_graph`. That covers `fno backlog done`, `fno done`, reconcile, the epic sweep, and `GraphTracker.close`. The Rust daemon shells out to `fno backlog done`. So the store's post-recompute hook is the one place a release covers every path. On a transition into a terminal rung the hook clears the entry's `locked_by`/`claimed_at` mirror. Terminal rungs are `done` and `superseded`, shared as `TERMINAL_RUNGS`. `session_id` stays: on a done node it is cost provenance, not a lock. The hook also force-releases the `node:<id>` claim at both claims roots, holder-agnostic, best-effort. The GitHub tracker backend closes an issue without touching the graph store. Its `close()` calls the same helper. A release failure is a named stderr line, never a failed closure. The reaper is the backstop.

**The sweep settles node-aware.** `sweep_verdict` is the single reap decision. It runs a settlement reading FIRST on every `node:` claim. A node the graph closed under the claim is positive abandonment. So is an expired lease whose holder's roster row resolves to a DIFFERENT node. Both are reaped. An unexpired lease is never settled away from a live holder. Every unreadable instrument answers unknown: graph, roster, absent row, unresolved node. Unknown KEEPS. The settlement travels with every producer: the hand-typed verb, the unattended reconcile sweep, the spawn guard's targeted reclaim. Reap apply also clears the graph lock mirror for reaped node claims. A reaped worker's node then stops reading `claimed` before `LOCK_TTL_HOURS` passes.

### Renewal re-anchors, and PID-reuse detection survives

`claims::renew` used to preserve the recorded pid while moving `expires_at`. A respawned worker renewing under a new pid then left a claim byte-identical to a dead worker's: dead pid, unexpired TTL, SUSPECT. Nothing on disk separated a live session from a corpse. So every reader that must not steal from the first was forced to protect the second.

When the recorded pid is dead and the claim is local, renewal now rewrites `pid`, `host`, `machine_id` and `acquired_at` together. Reuse detection compares `create_time(pid)` against `acquired_at`. It survives **because the anchor moves with the pid**. The new anchor postdates the holder's start, and a later recycle of that pid number still reads as reused. A live recorded pid is never rewritten.

The anchor is the DURABLE session pid from `fno claim session-pid`, never the renewing process. `fno-agents loop-check` exits about a second after it renews. Anchoring to it re-files the corpse under a fresh number and fixes nothing.

### A wedge fails, benign dedup does not

A refusal because a live worker holds the node is dedup: the desired end state is already true and a batch sweep must keep going. Exit 0. A refusal on a suspect claim, a corrupted one, or a reservation whose holder nothing can prove dead is a wedge. Nobody is building the node, and nobody will until someone intervenes. Exit non-zero, and the message names the clearing command. Both used to exit 0 while launching nothing.

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

## The worker must be able to write the claim store

If the worker cannot create the lockfile, the claim is not mutual exclusion. Every harness sandboxes writes to the launch cwd by default. fno's state lives outside it. So a spawned worker on a bounded posture writes nothing and holds no claim. `fno claim status node:<id>` then answers `free` while that worker is live, on its branch, doing the work. Any king reading the graph sees a free node and dispatches a second worker onto it. The standing rule "check the claim before manual node work" cannot catch this, because the check returns free.

This is not one harness's problem. codex `workspace-write` is the visible case. A claude worker fails the same way. When a personal `~/.claude/settings.json` grants `permissions.additionalDirectories`, it looks fine on that maintainer's machine. The repository ships no such file, so a fresh clone reproduces it.

**How the grant reaches the worker.** `fno.agents.writable_dirs.worker_writable_dirs()` computes the set per spawn. It passes through the `--add-dir` cell, which already maps natively for claude, codex and agy. fno never writes into a harness settings file (operator ruling `d-926a2b90`). The set is by need, never blanket, because `--add-dir` is a WRITE grant:

| Directory | When |
|---|---|
| The state root (`fno.paths.state_dir()`, plus the config-free claims root when an override splits them) | Always. No worker functions without the claim store, the graph and the ledger. |
| The plan directory | The directory holding this spawn's plan, never the vault above it. A worker writes its plan, not the operator's notes. |
| Sibling project roots | Only for a multi-repo wave, passed by the caller. |

Four funnels carry it and each is tested on its own lane. Three are Python token builders: the pane builder, claude's bg/headless builder, and codex's headless builder. The fourth is the spawn seam, and it is the one that matters most. `rust_runtime` keeps only the PANE substrate in Python. Every other spawn execs the `fno-agents` binary, which builds the harness argv itself and forwards only the operator's own `--add-dir`. So the three Python builders cover exactly one reachable lane. A `--substrate bg` spawn launched a worker with no write access to the claim store. That is the substrate the shipped stage table uses for its own delivery lane.

The seam publishes the computed set on `FNO_WORKER_ADD_DIRS`, which `os.execv` carries into the binary. An env var carries it rather than a repeated flag. `--add-dir` is scalar in both runtimes: typer types it `str | None`, and the Rust arg parser's `params.insert` overwrites. Widening it on both sides plus its parity mirror buys nothing the env channel does not, and the resolver stays in one language either way. A grant on one of several reachable paths is decorative.

**The provider with no additive grant.** opencode's `--dir` sets the working directory rather than adding one, so there is no cell to carry the set. An explicit operator `--add-dir` keeps its hard refusal there. The computed set is skipped with one named line on stderr instead. Fail-closed is right for something a human typed. For a default the caller never asked for, it is wrong: it refuses every opencode spawn.

**The half a spawn cannot reach.** A per-spawn grant says nothing about a session the operator started by hand, or one that joined by `/fno-me`. The first session on any machine cannot come from `fno agents spawn`. So `fno doctor` probes it. It writes a real file into the claim store and removes it. On failure it prints the remedy for the detected harness and exits nonzero. It advises and never writes. The probe writes rather than parsing settings files. Doctor runs inside the session being asked about, so it is the sample. An absence has two explanations. A completed write has one.

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

`fno.agents.reachability` (`cli/src/fno/agents/reachability.py`) answers two separate questions. It writes each answer to a separate field.

**`reachability`** answers: "Can the system reach this process?" Its values are `reachable` | `unreachable` | `unknown`. The `basis` field names the evidence: `transcript` | `process-gone` | `pane-gone` | `silent` | `no-evidence`. `fno agents list --status` filters the rendered values `live` | `orphaned` | `unknown`.

**`progress`** answers: "Is the worker advancing, waiting for the operator, parked, or refused?" Its values are `advancing` | `awaiting-operator` | `parked` | `refused` | `unknown`. The `progress_basis` field names the evidence: `transcript-turn` | `operator-turn` | `promise` | `model-refused` | `silent` | `no-evidence`. `advancing` requires a transcript advance in the ten-minute evidence window. An older `working` state returns `unknown` / `silent`. An unreadable activity age returns `unknown` / `no-evidence`. `fno agents list --progress` filters this axis independently of `--status`.

These questions need separate fields. A worker can be reachable while it advances, parks after completion, or uses an unsupported model. All three workers are reachable. Before this axis existed, all three rows displayed `live`. Thus, operators did not distinguish a staffed row from a stalled row. One scan showed 31 live processes that used 8513 MB.

A reading about one artifact is not a verdict about the agent. A missing transcript file proves only that the file is missing. Both axes must return `unknown` for this case. They must never report death, refusal, or parking from this absence.

When evidence is absent or unreadable, each falsifier in `reachability.py` returns `None`. `classify_progress` applies the same rule to the transcript model. Only classifiers in `reachability.py` can answer these axes. A reader must not derive liveness or progress from file existence.

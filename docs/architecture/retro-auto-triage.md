# Retro / auto-triage of left-out work

Work that is *decided but not done* during a PR used to evaporate: skipped reviewer suggestions, deferred-for-clarity decisions, and out-of-scope bugs found while building something else rarely became backlog nodes. This feature harvests those left-out items at merge time and turns each into an *expanded* backlog node (or a queued draft, or an inbox line), deduped against existing nodes.

## Data flow

```
in-session:  executor --fno carveout add--> .fno/carveouts.jsonl
                                                     |
merge (ship gate):   pr-merge.sh --> .fno/.triage-pending  (fast-path, /target only)
merge (outside):     reconcile  --> ~/.fno/retro-pending/<node>.json  (universal)
                                                     |
no trigger at all:   `fno retro sweep-carveouts` reads the ledger directly
                                                     |
                              `fno retro run` (consumer)
                                                     v
   shared routine (cli/src/fno/retro/routine.py::triage_pr):
     harvest (carveouts + declined reviews + COMPLETION deferred_findings)
       -> classify + expand (verbatim reasoning + source cite, severity tier)
       -> dedup (source_pr + content-hash, idempotent)
       -> land: autonomous? fno backlog new (active)
                interactive? fno backlog new + queue  (adopt-stays-pure)
                low/nit?     backlog.inbox.add_item (fu- line)
```

## Components

### Wave 1 — reconcile auto-trigger

`fno backlog reconcile` closes nodes whose PR merged outside the ship gate and drops a retro sentinel, but nothing invoked it. Two throttled surfaces now do:

- **`hooks/reconcile-session-start.sh`** (SessionStart): renders the *prior* sweep's result as a reminder (only when a node was closed), then launches a fresh reconcile detached via `nohup` so session start never blocks.
- **`hooks/megawalk-stop-hook.sh`** (between iterations): fires the same throttled reconcile so long autonomous runs reconcile without a fresh session.

Both source `scripts/lib/reconcile-throttle.sh` and share one throttle stamp (`.fno/.reconcile-stamp`, ~15 min, `RECONCILE_THROTTLE_SECONDS` override) so parallel sessions don't hammer `gh`. Reconcile always runs in mutate mode here — writing the retro sentinel is the point. AGENTS.md documents the `/loop 30m fno backlog reconcile` cadence for non-megawalk terminals.

### Wave 2 — carve-out capture (`fno carveout add`)

A session-time capture primitive (NOT a backlog mutation — Locked Decision #10). The executor calls it the moment it leaves work undone:

```
fno carveout add --kind deferred|oos-bug [--need "<open question>"] [--priority pN] "<what + why>"
```

It appends one JSON line to `.fno/carveouts.jsonl` via the events.jsonl mkdir-mutex convention. `session_id` resolves from `target-state.md` then `$CLAUDECODE_SESSION_ID`; a missing session records unscoped (exit 0 + stderr warn) so capture is never lost. A failed write exits non-zero (no silent success). The instruction lives in the `using-fno` preamble so every pipeline (`/target`, `/do` (incl. waves), `/goal`, loops) emits carve-outs. Advisory, not gate-enforced — the merge-time harvest is the backstop.

### Waves 3-4 — the shared retro-triage routine

`cli/src/fno/retro/`:

| Module | Role |
|---|---|
| `harvest.py` | gather carve-outs + declined reviewer findings (gh-injectable; severity-badge normalized, no-badge → medium; resolved/fix-commit ids subtracted) + COMPLETION.md `deferred_findings`. gh failure → WARN + process the rest; malformed jsonl line skipped, never aborting. |
| `classify.py` | verbatim reasoning + source cite; title from the finding's own first line (never a generic stub); severity → tier (crit/high/med → node, low → inbox) and → priority; **uncited candidates rejected** (anti-hallucination); body truncated to a cap with a marker. |
| `dedup.py` | key = `source_pr + blake2b(normalized finding)`; badge/whitespace-insensitive so two reviewers on one issue collapse; reads existing keys from a machine trailer in node `details` (no schema fields). Idempotent. |
| `land.py` | routes by mode — autonomous → active node; interactive → create + queue (adopt-stays-pure); low/nit → `backlog.inbox.add_item`. Mode from the trigger sentinel, absent → interactive (safe). Per-node failures recorded, not raised, so partial progress persists and a re-run dedups. |
| `routine.py` | the one shared `triage_pr(...)` both triggers call. |
| `sweep.py` | the PR-independent ledger sweep (see below): reads `.fno/carveouts.jsonl` with no trigger, dedups every row against the live graph, files only what nothing already tracks. `plan_sweep` is pure, so the dry run and the applied run cannot diverge. |
| `cli.py` | `fno retro run` consumes `~/.fno/retro-pending/*.json` (universal) and `.fno/.triage-pending` (fast-path). Consume-then-remove: a sentinel is removed only on a clean land; a partial harvest (`gh_unavailable`) or land failure retains it for retry. Reloads live nodes per sentinel so dual triggers collapse to one node set. |

### The PR-independent sweep (`fno retro sweep-carveouts`)

Everything above is PER-PR: a trigger names a PR, and the carve-out read is scoped to that PR's owning session(s) from `ledger.json`.
Two shapes of carve-out fall outside every one of those triggers.
A carve-out written with no resolvable session (`session_id: null`) never matches a session scope, and an unresolvable owner degrades to read-only rather than filing.
A PR that merged without ever dropping a sentinel leaves nothing to iterate, so `fno retro run` returns "no retro-pending sentinels to triage" and never opens the ledger.
Both accumulate silently, and since the close gate's condition D refuses a close on any unharvested `deferred` carve-out, they eventually wedge unrelated nodes.

`fno retro sweep-carveouts` closes that gap by reading the ledger itself, keyed off nothing.
It is a DRY RUN by default; `--apply` is the only path that writes.
Bare `fno retro run` reports the pending count on every one of its callers (the SessionStart reconcile throttle, `/fno:pr check`, `/fno:pr merged`, direct CLI) but never applies, and neither does `fno backlog groom`.

**The harvest is manual by design, and that is the trade, not an oversight.**
There is no `fno backlog delete`, so every filed node is permanent.
An irreversible operation must not run unattended, and `fno retro run` fires from a background SessionStart hook.
The measured consequence of that rule: condition D keeps refusing closes until a human runs the verb.
A gate that clears itself unattended by minting permanent state is not a gate, and a harvest that files a duplicate has no undo.
One project reached 23 surplus nodes that cannot be removed by taking the other side of this trade.

Dedup is the feature, not polish. Filing one node per carve-out is worse than not harvesting, because a re-filed item becomes a duplicate and there is no `fno backlog delete`.
Each row is matched against every node in the graph, done nodes included, by three matchers with two outcomes:

| Match | Outcome |
|---|---|
| cv-id quoted verbatim in a node's title or details (**exact**) | `resolve`: consume the row, file nothing, name the tracking node. Every node the sweep files cites its own cv-id, so a re-run is a no-op. |
| an existing `retro-triage` trailer whose `finding_hash` equals the description hash, **ignoring** the trailer's `source_pr` (**exact**) | `resolve`: an earlier PR-scoped harvest already filed it. |
| normalized-title similarity at or above 0.85, minimum 20 chars (**fuzzy**) | `review`: neither filed (would duplicate) nor consumed (a wrong guess loses the work). A human decides, and a parked `deferred` row keeps blocking its close, which is the correct outcome. |
| nothing matched | `file`: mint the node (queued behind `fno backlog pick` in interactive mode), then consume. |

No row is ever consumed without a node id attached to it.
A failed mint leaves the row in the ledger; clearing it to turn the gate green with the work tracked nowhere is exactly what the gate exists to prevent.

`deferred` and `oos-bug` are both swept and stay distinct: `deferred` is declared scope that did not ship and blocks a close, `oos-bug` is discovery and never blocks.
`backfill` is skipped here as it is everywhere else, since it belongs to `/fno:pr merged`'s backfill slot.

### Classification is deterministic by design

Discretion #4 asked to "keep the classify step LLM-driven." We resolved this by *hosting*, not by a hidden API call: `classify.py` is deterministic and verbatim-preserving, which is the stronger anti-hallucination guarantee — with no LLM summarization there is no surface to fabricate a finding ("no cite, no node" is mechanical). The routine runs at an LLM-present checkpoint (the `/target` post-merge fast-path or the sentinel consumer) that *can* review before landing.

## Boundary with the memory pass

The post-merge **memory** pass writes *lessons* (`reference_*`/`feedback_*` entries). Retro-triage writes *actionable work* (backlog nodes). They run at the same checkpoint and read overlapping sources, but emit to different artifacts; one reviewer comment can legitimately produce both.

## Known limitation (tracked)

`harvest_reviews` accepts `resolved_ids`/`skipped_ids` but the consumer does not yet *derive* them from real PR data (resolved review-thread state + the author's "Skipped" table). Until that lands, implemented reviewer findings can be re-filed in autonomous mode; interactive mode mitigates via the human queue-ack. Tracked as a deferred carve-out from the initial PR.

# Retro / auto-triage of left-out work

Work that is *decided but not done* during a PR used to evaporate: skipped reviewer suggestions, deferred-for-clarity decisions, and out-of-scope bugs found while building something else rarely became backlog nodes. This feature harvests those left-out items at merge time and turns each into an *expanded* backlog node (or a queued draft, or an inbox line), deduped against existing nodes.

## Data flow

```
in-session:  executor --fno carveout add--> .fno/carveouts.jsonl
                                                     |
merge (ship gate):   pr-merge.sh --> .fno/.triage-pending  (fast-path, /target only)
merge (outside):     reconcile  --> ~/.fno/retro-pending/<node>.json  (universal)
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
| `cli.py` | `fno retro run` consumes `~/.fno/retro-pending/*.json` (universal) and `.fno/.triage-pending` (fast-path). Consume-then-remove: a sentinel is removed only on a clean land; a partial harvest (`gh_unavailable`) or land failure retains it for retry. Reloads live nodes per sentinel so dual triggers collapse to one node set. |

### Classification is deterministic by design

Discretion #4 asked to "keep the classify step LLM-driven." We resolved this by *hosting*, not by a hidden API call: `classify.py` is deterministic and verbatim-preserving, which is the stronger anti-hallucination guarantee — with no LLM summarization there is no surface to fabricate a finding ("no cite, no node" is mechanical). The routine runs at an LLM-present checkpoint (the `/target` post-merge fast-path or the sentinel consumer) that *can* review before landing.

## Boundary with the memory pass

The post-merge **memory** pass writes *lessons* (`reference_*`/`feedback_*` entries). Retro-triage writes *actionable work* (backlog nodes). They run at the same checkpoint and read overlapping sources, but emit to different artifacts; one reviewer comment can legitimately produce both.

## Known limitation (tracked)

`harvest_reviews` accepts `resolved_ids`/`skipped_ids` but the consumer does not yet *derive* them from real PR data (resolved review-thread state + the author's "Skipped" table). Until that lands, implemented reviewer findings can be re-filed in autonomous mode; interactive mode mitigates via the human queue-ack. Tracked as a deferred carve-out from the initial PR.

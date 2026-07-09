---
name: triage
description: "Propose and apply optimal ordering for pending specs on the graph backlog. LLM proposes, human approves. Use when: 'triage the backlog', 'what should I work on next', 'reorder my specs', 'find duplicates in graph'."
argument-hint: "[deep] [all] [each] [dry-run] [--project NAME] [--roadmap-id ID]"
---

# Triage

Propose an optimal ordering for pending specs on `~/.fno/graph.json`
based on dependencies, sequencing, priority versus project goals, and
duplicate detection.

## Philosophy

- **LLM proposes, human approves.** Triage never auto-applies priority
  changes — priority is a business decision.
- **Shallow by default.** Metadata-only reasoning keeps token cost low
  for large backlogs. Use `deep` when you want richer inference from
  the plan contents.
- **Cycles are dropped automatically.** If a proposed dependency would
  create a cycle (A blocks B, B blocks A), validation drops the offending
  edge and logs the conflict.
- **Project-scoped by default.** Triage filters to the current repo's
  project (matched by canonical repo root, stable across worktrees).
  Pass `all` to triage across every project or `--project <name>` to
  target a specific one. Matches the scoping of `roadmap-tasks.py
  next`/`ready`.

## Setup

Project scoping requires at least one node registered to the current repo.
The first `/blueprint` with auto-intake (or a manual `roadmap-tasks.py intake`)
stores the canonical repo root as the node's `cwd` and derives a project
name from the repo directory basename. Subsequent triage runs from the
same repo — including from any git worktree of it — auto-detect that
project.

If no project is detected (no adopted node yet for this repo), triage
falls through to `all projects` and prints a hint in the scope line so
the behavior is visible, not silent.

## Invocation

Positional modifiers (boolean switches): `deep`, `all`, `dry-run`.
Value-carriers (flags with arguments): `--project NAME`, `--roadmap-id ID`.

| Form | Effect |
|------|--------|
| `/triage` | Shallow triage, scoped to the current project |
| `/triage deep` | Read the first 150 lines of each plan_path for richer inference |
| `/triage all` | Include pending nodes from every project |
| `/triage each` | Iterate through every project with pending nodes, running the full pipeline and per-project approval once per project |
| `/triage deep all` | Combine: cross-project, read plan contents |
| `/triage --project <name>` | Scope to a specific project by name |
| `/triage --roadmap-id rm-X` | Triage only nodes tagged to a specific roadmap |
| `/triage dry-run` | Print candidates only, no LLM call, no mutations |

### Invocation surface (CLI-source-agnostic)

The skill issues triage verbs through whichever implementation is
available. Preference order:

1. `fno backlog triage <verb> <flags>` - the v2 CLI (canonical)
2. `python3 scripts/triage.py <verb> <flags>` - the shim, which itself
   forwards to v2 when `fno` is on PATH or falls back to the in-repo
   module via `cli/src`

Both satisfy the same JSON protocol - callers see identical
input/output. Every `scripts/triage.py …` command in the steps below is
equivalent to `fno backlog triage …`; prefer the latter when `fno` is
on PATH so the call path is one process shorter.

### Translating positional modifiers to the underlying command

Both implementations use standard argparse/Typer conventions
(`--deep`, `--all`, `--dry-run`). When executing a user invocation,
translate positional words into the equivalent flag:

| User typed | Pass through |
|------------|--------------|
| `deep` | `--deep` |
| `all` | `--all` |
| `dry-run` | `--dry-run` |
| `each` | (no direct flag; orchestrates multi-run invocation in the skill, not the command) |
| `--project foo` | `--project foo` (unchanged) |
| `--roadmap-id rm-X` | `--roadmap-id rm-X` (unchanged) |

### Mutually-exclusive and composition rules

- `each` combined with `--project <name>` is an error. The skill must
  refuse this combination with the exact message
  `"each and --project are mutually exclusive: --project scopes to one, each iterates many"`
  and exit without calling the script. Confirms before any graph
  mutation.
- `each` combined with `all` is redundant (each is stricter). Silently
  prefer `each` and note in the scope line printed to the user.
- `each` composes normally with `deep`, `dry-run`, and `--roadmap-id`.
  Those modifiers forward into every per-project invocation.

## Process

1. **Gather context.** Run the context generator to get pending nodes
   plus project goals from config.toml. Forward the user's positional
   modifiers as the matching flags (e.g. `deep` -> `--deep`):

   ```bash
   # v2 preferred; shim used when `fno` is not on PATH
   (fno backlog triage context [--deep] [--all] [--project NAME] [--roadmap-id ID] 2>/dev/null \
      || python3 scripts/triage.py context [--deep] [--all] [--project NAME] [--roadmap-id ID]) \
     > /tmp/triage-ctx.json
   ```

   The context JSON has two top-level arrays:

   - `candidates`: claim-ready rows (`_status: ready` or `blocked`).
     These are what the LLM should reason about for ordering, blocker
     edges, and priority changes.
   - `ideas`: plan-less rows (`_status: idea`). These are NOT claimable
     work - they are captured thoughts waiting for a spec. The LLM
     should mention them in its summary with the recommendation
     "write a spec for this idea before claiming it" rather than
     proposing dependency edges or claim ordering. Run `/blueprint` against
     the idea's title + details, then `fno backlog intake <plan_path>`
     to flip the node to `_status: ready`.

   If `candidates` is empty AND `ideas` is empty, print "no pending
   nodes to triage" and exit. Done. If `candidates` is empty but
   `ideas` is non-empty, surface the idea list to the user with the
   spec-it-first recommendation and exit (no LLM reasoning needed -
   there is no claim-ordering decision to make until a spec lands).

2. **LLM reasoning.** Spawn a subagent (Task tool) with the context JSON
   and this instruction. Run the subagent at **temperature 0** - triage is a
   classification task and determinism drives the 95%+ consistency target
   (`fno backlog triage consistency` measures it).

   > You are a backlog triage classifier. First **reason**, then **label** -
   > this ordering (analysis before the JSON) is what makes the classification
   > consistent, so never emit the JSON first.
   >
   > **First**, in a short `## Reasoning` section, work through the candidates:
   > which specs block which, which priorities are misaligned with the project
   > goals, which are stale or duplicated. Name the primary concern of each spec
   > you touch. When a spec raises several concerns at once, classify on its
   > *primary* intent, not the loudest surface signal.
   >
   > **Then**, output an optimal ordering as JSON with four keys:
   >
   > - `dependencies`: edges where one spec must complete before another
   >   (e.g. `{"from": "ab-X", "to": "ab-Y", "reason": "..."}` means Y is
   >   blocked_by X).
   > - `priority_changes`: spec priority adjustments that align better
   >   with the project's stated goals.
   > - `defer`: nodes to pause with `{"id": "ab-X", "reason": "..."}`.
   >   Use when a node is stale, waiting on an external decision, or
   >   otherwise not suitable for the current cycle. Defer is reversible
   >   via `fno backlog undefer <id>`. Reason is required and is surfaced
   >   in the kanban view alongside the card.
   > - `duplicates`: specs that look like near-duplicates, with a
   >   `recommended` action of `"merge"` and a reason. To pause one of a
   >   pair without merging, use the `defer` action above instead.
   >
   > Every entry MUST include a `reason` field (one line) referencing the plan
   > contents (deep mode) or the title/priority signal (shallow mode). An entry
   > without a reason is dropped by validation, so a missing reason is a wasted
   > proposal. Do not propose self-edges or edges that create cycles.
   >
   > **Field reference:** each candidate carries enrichment fields beyond
   > the basic id/title/priority. `size` is S/M/L (omit size-based
   > reasoning when null). `domain` is the execution domain
   > (code/research/etc). `details` is the user's implementation guidance
   > captured at intake; cite it for shallow-mode reasoning instead of
   > paying for a deep read. `claim_history.session_count > 1` means the
   > node has been claimed and released before; `claim_history.total_cost_usd`
   > is cumulative spend without shipping. `ship_state.pr_number` set means a PR exists - the node
   > may already be done and just needs a `done` flip rather than
   > re-triaging. `ship_state.merge_status` carries the PR's merge state
   > (merged/closed/null).
   >
   > IMPORTANT: only reason over the `candidates` array. The `ideas`
   > array contains plan-less rows that are not claimable yet - do NOT
   > propose dependencies or priority changes for ideas. Pass them
   > through unchanged in your summary so the user sees them and can
   > spec them next.
   >
   > **Examples** (reasoning before label; classify on primary intent):
   >
   > - *Implicit dependency.* Candidate `ab-9f` "add rate-limit headers to the
   >   API" and `ab-3c` "build the API gateway". The gateway is never named as a
   >   blocker, but headers cannot ship before the gateway exists.
   >   Reasoning: ab-9f's work sits on top of ab-3c's surface.
   >   Label: `dependencies: [{"from":"ab-3c","to":"ab-9f","reason":"rate-limit headers need the gateway surface first"}]`.
   > - *Emotion over intent.* Candidate details read "this flaky test has wasted
   >   HOURS, it is infuriating, the whole suite is garbage." The heat is noise;
   >   the actionable intent is one flaky test.
   >   Reasoning: strip the frustration, the core ask is de-flaking one test - a
   >   normal p2, not a p0 because it is loud.
   >   Label: no priority bump; a `defer` only if it is blocked on an external fix.
   > - *Multiple issues, pick the primary.* Candidate details raise a perf
   >   regression, a typo in a log line, and a docs gap. The perf regression is
   >   the reason the node exists; the rest are incidental.
   >   Reasoning: classify on the perf regression (the primary), mention the
   >   others in the summary, do not split the node.
   >   Label: priority reflects the perf regression only.

   The subagent returns proposal JSON.

   **Tournament ordering (large backlogs).** When `count` (from `triage
   context`) is large enough that a one-shot ordering gets unreliable -
   roughly `count >= 6` - prefer comparative judgment over absolute scoring
   for the *claim-order* decision: ask the LLM to pick a winner per candidate
   PAIR ("ship X or Y first?") rather than rank all at once. Enumerate the
   candidate pairs, collect each verdict as `{"winner": "ab-X", "loser":
   "ab-Y"}`, write them to `/tmp/triage-verdicts.json`, then fold them into a
   single consistent order:

   ```bash
   fno backlog triage rank --verdicts /tmp/triage-verdicts.json
   ```

   It aggregates by Copeland score (wins minus losses), tolerating the
   occasional contradictory or cyclic verdict, and emits a best-first `order`.
   Apply that order to the board so board position matches work order: seed the
   top item with `fno backlog rank <first-id> --top`, then chain each remaining
   id after its predecessor with `fno backlog rank <id> --after <previous-id>`.
   Do NOT call `--top` for every id in a forward pass: `--top` inserts before
   the current front of the lane, so iterating best-first would reverse the
   order (the last id processed would land first). Feed the ranking into the
   `priority_changes` rationale too. For a small backlog the one-shot proposal
   above is cheaper and fine; reserve the pairwise pass for when the node count
   makes it worthwhile.

3. **Validate.** Run:

   ```bash
   (fno backlog triage validate /tmp/triage-proposal.json 2>/dev/null \
      || python3 scripts/triage.py validate /tmp/triage-proposal.json) \
     > /tmp/triage-cleaned.json
   ```

   Stderr surfaces cycles, unknown IDs, or invalid priorities that were
   dropped. Stdout is the cleaned proposal.

4. **Present to user via AskUserQuestion.** Show a summary of the
   proposed changes with per-item rationale. Offer:

   - **Approve all** — apply every edge, priority change, and flag every
     duplicate
   - **Pick** — select a subset to apply (pass `--pick id1,id2,...` to
     `apply`)
   - **Reject** — discard the proposal; exit 0

5. **Apply.** On approve or pick:

   ```bash
   fno backlog triage apply /tmp/triage-cleaned.json [--pick id1,id2] \
     || python3 scripts/triage.py apply /tmp/triage-cleaned.json [--pick id1,id2]
   ```

   Apply runs inside a single `locked_mutate_graph()` so `graph.md`
   re-renders once with all mutations. Report the applied count plus any
   flagged duplicates for the user to resolve manually.

6. **Suggest next step.** If any dependencies or priorities changed,
   invite the user to run `/target` to pick the now-correctly-ordered top
   node.

## Iteration Mode (`each`)

When `each` is present, the skill orchestrates the Process pipeline once
per project with pending nodes, preserving project boundaries so the LLM
never reasons across unrelated repos. This makes it structurally
impossible to propose cross-project dependency edges or flag
cross-project duplicates - the reasoner simply never sees the other
project's candidates.

1. **Enumerate.** Call `fno backlog triage projects` (or
   `python3 scripts/triage.py projects` if `fno` is not on PATH) to get
   the list of projects with at least one pending node.

2. **Guard.** If the list is empty, print `"no pending nodes in any
   project"` and exit 0. Done.

3. **Iterate.** For each project (in the order returned by the script):

   a. Print a scope banner: `"=== Triaging project '{name}' ({count} pending) ==="`
   b. Run the existing Process steps 1-5 scoped to that project by passing
      `--project {name}` on every script invocation.
   c. Forward any composable modifiers from the outer invocation:
      `deep` -> `--deep`, `dry-run` -> `--dry-run`, `--roadmap-id X` -> `--roadmap-id X`.
   d. Present the proposal via AskUserQuestion as in the single-project
      flow. On Approve or Pick, call apply scoped to the project. On
      Reject, skip that project's apply and advance to the next.
   e. Accumulate per-project applied counts for the final summary.

4. **Summarize.** After the loop completes, print a single summary line:
   `"Triaged N projects: X edges applied, Y priority changes, Z duplicates flagged."`
   Include a breakdown per project if any project errored.

### Failure handling

If a per-project invocation fails (context generation returns an error,
proposal JSON is malformed, apply hits a locked-mutate conflict), log the
error to stderr with the project name, mark that project as `errored` in
the summary, and continue to the next project. Never abort the whole
iteration because one project failed. The user can re-run
`/triage --project {name}` later to address the single failure.

### Cost profile

One LLM reasoning call per project. For N projects with M average pending
nodes each:

- Shallow: ~0.5 KB/node -> per-project LLM call ~M * 0.5 KB input
- Deep: ~5 KB/node -> per-project LLM call ~M * 5 KB input

Total tokens scale linearly with project count. For users with >10 active
projects, prefer `each` over `all deep` - the sum of N small calls is
usually cheaper than one call over the union, and the LLM reasons better
on smaller focused sets.

## Token Cost

| Mode | Bytes per node | 20-node backlog |
|------|----------------|-----------------|
| Shallow (default) | ~0.5 KB metadata | ~10 KB total |
| Deep (`deep`) | ~5 KB with 150-line excerpt | ~100 KB total |

Chunk into two passes if pending count exceeds 50 in deep mode.

## Integration with `/target`

When `/target` is invoked with no arguments, the wizard offers
"Pick from backlog" as an input type. That path runs this skill
inline, presents the top-ranked ready node with rationale, and on
confirmation execs `/target {plan_path}` for that node.

## Out of Scope

- Fully automated triage (no human in the loop). Intentionally rejected.
- Cross-roadmap priority comparison. Single-roadmap or all-pending only.
- Auto-merging detected duplicates. Flagged for human resolution.
- Auto-undeferring on a calendar trigger (time-bounded defer). Defer is
  reversed manually via `fno backlog undefer <id>` for now.
- Learning from past decisions. Prompt-only reasoning for now.

## Health monitoring (background, deterministic)

`/triage` itself is interactive and LLM-driven. For continuous backlog
hygiene without human attention, use the deterministic check:

```bash
# Hourly local check; produces no output unless something is wrong
/loop 1h fno backlog triage health --check --quiet
```

Exit code 4 indicates a threshold breach (idea pile, stale-ready,
failure-prone, or collisions). Configurable via
`config.health_monitor` in config.toml; full schema in
[CLAUDE.md > Backlog Health Monitoring](../../CLAUDE.md#backlog-health-monitoring-2026-04-27-plan-ab-571c072b).
The monitor is pull-based (poll every interval) and never auto-mutates
the graph; breaches notify and the human (or a follow-up `/triage`)
decides what to do.

## References

- `fno backlog triage` (canonical) — the v2 CLI sub-app; source in
  `cli/src/fno/graph/triage.py`
- `scripts/triage.py` — compatibility shim that forwards to the v2 CLI
  (or falls back to the in-repo module when `fno` is not on PATH)
- `cli/src/fno/graph/store.py` — `locked_mutate_graph()` entry point
- `~/.fno/graph.md` — the kanban view that reflects applied changes
- `cli/src/fno/health_monitor.py` — threshold evaluation,
  notification dispatch, history append/read, trend summary

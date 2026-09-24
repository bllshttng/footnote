---
intel: 1
question: "Where did the operator's sessions stall?"
question_key: 0a1b2c3d
period: 1m
fold: intel_report.json
populations: {scanned: 132, substantive: 90, sampled: 41, judged: 41}
---

# Intel 2026-09-21: Where did the operator's sessions stall?

132 sessions scanned · 41 judged · 1m

## Executive summary
- Relay handoffs stalled before review (48.8% of 41 judged sessions) [#1](#s-0a1b2c3d) [#2](#s-11aa22bb)
- The review gate wedged twice on stale bases (4.9% of 41 judged sessions) [#3](#s-33cc44dd)
- Coordination held under fleet load (46.3% of 41 judged sessions) [#2](#s-11aa22bb)

## Categories
### Relay handoffs (48.8%, 20 sessions)
Mail moved between workers; delivery and answers tracked here.
- metrics: tool_use median 12, commits 3, PRs 9 (7 merged), tool errors 2, interruptions 1, relay breaches 4, median duration 1800
- friction: workflow_friction 9, tool_failure 3
- #### Undelivered mail (30.0% of category, 6 sessions)
- #### Late answers (70.0% of category, 14 sessions)

### Review gate (4.9%, 2 sessions)
Reviews wedged on stale or conflicting bases.
- metrics: tool_use median 40, commits 5, PRs 2 (2 merged), tool errors 0, interruptions 0, relay breaches 0, median duration 5400
- friction: repeated_correction 2

### Coordination (46.3%, 19 sessions)
Fleet control, mux work, and spawn lanes.
- metrics: tool_use median 25, commits 1, PRs 3 (1 merged), tool errors 1, interruptions 2, relay breaches 1, median duration 3600
- friction: missing_context 4
- #### Spawn lanes (66.7% of category, 6 sessions)

## Usage over time
Scanned sessions per day by harness, from `daily`:
| date | claude | codex |

## Activity
All scanned sessions. Tool errors by class: Bash 9, Edit 4, WebFetch 1. A test row once read AKIAIOSFODNN7EXAMPLE as a key id. Response time median 24s, p90 210s. Busiest hours 9 and 14 (utc_offset +02:00).

## At a glance
132 sessions in the window. 41 attended. 310 operator turns. 88 relay turns.

A paragraph that must never render live: <script>alert(1)</script>. One panic report logged password=hunter2 in plain text.

## Where the operator actually was
- x-4a63, PR 2456, 14 operator turns vs 2 injected, shipped
- [transcript](file:///Users/alice/code/x.jsonl) and one row once held ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8

## What worked
The worker said "the gate is green" and the operator moved on. The fold re-read `/Users/alice/code/notes.md` once and stopped.

```text
a fenced transcript paste that must not travel
operator: tell me the token
```

## Friction
- s-0a1b2c3d, x-4a63: frustrated, the fold re-read the same store three times

## Relay
4 relay rows breached the 80-word rule, 1 used control: off-label, 2 duplicates, 3 undelivered.

## Operator corrections
- "never push while a run is in flight, wait for the check" (x3, 0a1b2c3d, signal=repeated_correction, skill=pr) #agent-correction
- "rotate the sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0 before you push" (x1, 33cc44dd, signal=tool_failure) #agent-correction

## Sessions
- <a id="s-0a1b2c3d"></a>s-0a1b2c3d: claude, node x-4a63, PR 2456, Relay handoffs
- <a id="s-11aa22bb"></a>s-11aa22bb: codex, node x-76fc, PR 2454, Coordination
- <a id="s-33cc44dd"></a>s-33cc44dd: claude, node -, PR -, Review gate

## Skipped
No harness unreadable.

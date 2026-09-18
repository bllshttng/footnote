# Report shape

One markdown report per run: `<vault>/fno/intel/<date>.md`. The section skeleton mirrors Claude's `/insights` narrative plus the two sections Claude cannot write (relay, corrections). Numbers come from the fold (`fno-agents intel --json`); judgments come from the operator-turn facets only.

```markdown
# Intel <date>

## At a glance
Four lines, one number each: sessions in window, attended sessions,
operator turns, relay turns. (Fold totals.)

## Where the operator actually was
The attended sessions with their nodes and PRs. One line each:
node, PR, operator turns vs injected ones, outcome from the facet.

## What worked
Sessions the operator was satisfied with; name the behavior, not the vibe.
Judged from operator turns only.

## Friction
Frustrated and mixed sessions, grouped by friction category, one line each
with the session and node named. The category is the facet's `friction` field.

## Relay
The mail graph, no model: rows the fold's `nodes[]` carries - per node,
unanswered count, median reply latency, longest silence. Then the contract
line: how many relay rows breached the 80-word rule, how many used
`control:` off-label, how many were duplicates, and the undelivered count.

## Operator corrections
Verbatim, deduped across sessions, ranked by repeat count:

- "<correction>" (x<repeat count>, <session prefix>, signal=<category>) #agent-correction

After the list, one line per correction saying whether it wants to be an
AGENTS.md line or a law. This section is the only one the S2 writer reads.

## On the horizon
One paragraph: what the operator's last 14 days say to do next. From
operator turns only; no relay or harness turn may appear here.

## Skipped
The harnesses the fold reports under `skipped` (today: opencode, no
transcript source). Counted, never guessed.
```

Rules the renderer holds to:

- Every number in the report traces to the fold's JSON. If it is not in the JSON, it is not a number, it is a claim - and it does not get digits.
- Unattended sessions (zero operator and zero relay turns) stay out of every section except the fold counters; they are machinery runs.
- The corrections lines must survive `bash scripts/corrections-insights-tag.sh --insights-file <report>` untouched: the tag is ` #agent-correction` at end of line, the `signal=` pair inside it.

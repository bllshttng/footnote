# Report shape

One markdown report per run: `<vault>/fno/intel/<date>-<question key>.md`. The section skeleton is Claude's `/insights` narrative adapted, plus the two sections Claude cannot write (relay, corrections). Numbers come from the saved fold JSON named in the frontmatter `fold:` field. Judgments come from the facets of the sampled sessions only. Populations never blend: fold counters, Usage over time, and Activity speak for the `scanned` population; the Executive summary, Categories, and the facet sections speak for the `judged` one.

The fold JSON fields the report and the renderer read: `populations`, `activity`, `hours`, `response_time`, `parallel`, `daily` (entries `{date, harness, sessions, operator_turns, tool_use, output_tokens}`, dates in the fold's local time), `daily_undated`, `sample`, `categories`.

```markdown
---
intel: 1
question: "<text>"
question_key: <8 hex>
period: <2w|1m|2m|3m|all>
fold: <date>-<question key>.json      # the saved fold JSON, same dir
populations: {scanned: <n>, substantive: <n>, sampled: <n>, judged: <n>}
---

# Intel <date>: <question>

<scanned> sessions scanned · <judged> judged · <period>

## Executive summary
- <finding that answers the question> (<share_pct>% of <judged> judged sessions) [#1](#s-<8 hex>) [#2](#s-<8 hex>)
(3 to 6 lines. Every share is a category or subcategory share_pct.)

## Categories
### <category name> (<share_pct>%, <sessions> sessions)
<description>
- metrics: tool_use median <m>, commits <c>, PRs <with_pr> (<merged> merged), tool errors <e>, interruptions <i>, relay breaches <r>, median duration <d>
- friction: <word> <n>, ...
- #### <subcategory name> (<share_pct>% of category, <sessions> sessions)

## Usage over time
Scanned sessions per day by harness, from `daily`:
| date | claude | codex | opencode |

## Activity
All scanned sessions. Tokens (input, output, cache read, cache write), lines added and removed, tool errors by class, interruptions, top 8 languages by extension, response time median and p90, busiest operator hours (with utc_offset), parallel sessions (overlap_pairs, sessions). Harnesses under `activity.unmeasured` are named.

## At a glance
Four lines, one number each, all `scanned`: sessions in window, attended sessions, operator turns, relay turns. (Fold totals.)

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

- "<correction>" (x<repeat count>, <session prefix>, signal=<category>, skill=<name> when the correction is about one fno verb) #agent-correction

After the list, one line per correction saying whether it wants to be an
AGENTS.md line, a law, or a SKILL.md diff. This section is the only one
the S2 writer reads.

## On the horizon
One paragraph: what the operator's last 14 days say to do next. From
operator turns only; no relay or harness turn may appear here.

## Sessions
- <a id="s-<8 hex>"></a>s-<8 hex>: <harness>, node <id|->, PR <n|->, category <name>

## Skipped
The harnesses the fold reports under `skipped`: harnesses whose store
could not be read, with the reason. Counted, never guessed.
```

Rules the renderer holds to:

- Every number in the report traces to the saved fold JSON named in `fold:`. Every number names its population: `scanned` for fold counters, Usage over time, and Activity; `judged` for the Executive summary, Categories, and the facet-derived sections. A line never mixes two.
- Anchors are `s-` plus the first 8 characters of the session id, and only sessions named in the run file get a Sessions line.
- The new sections carry no transcript text beyond the one-line summaries.
- The tag `#agent-correction` appears only in the Operator corrections section. The corrections line grammar does not change.
- A section that cannot be written still appears, with one line `not written: <reason>`. It is never left out.
- Unattended sessions (zero operator and zero relay turns) stay out of every section except the fold counters. They are machinery runs.
- The corrections lines must survive `bash scripts/corrections-insights-tag.sh --insights-file <report>` untouched: the tag is ` #agent-correction` at end of line, the `signal=` pair inside it.

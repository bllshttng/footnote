# Answerer Enumeration

One question drives a feature. Several places in the tree answer it. A plan that fixes the site where the symptom appeared leaves the other answerers standing. The feature then returns as a second PR, a third, and the eight-PR pattern this protocol exists to end. Count the answerers before the PRs do.

`/blueprint` runs this protocol as step 2b-bis, after discovery grounding (2b) and before the Consolidation Gate (2d). The gate contract, the refusal strings, and the graduation live in [blueprint-gates.md](blueprint-gates.md). The validator teeth live in `scripts/validate-plan.sh` beside the Consolidation Gate. This file teaches the four steps and the sweep rules that make their output trustworthy.

## The unit is a question, and the count is the answerers

Phrase the unit as a question, never as a noun phrase. "Is this row reachable?" forces the count into the open. "Row reachability" hides it. The answerer count is the PR estimate, available at plan time. A question with four answerers is a four-PR feature, or a one-PR feature with a four-answerer plan. State the count and the operator chooses between them. Today that number surfaces one PR at a time, after the fact.

The inverse failure exists too: one answerer conflating two questions. A change you cannot phrase as one question is answering two. Split it and run this protocol twice.

## Step 1 - Phrase the question in one line

Write it as a question that ends in a question mark. "Was this PR reviewed?" "Which nodes need dispatch?" Never prose. Never a noun phrase. The validator refuses a `question:` without the question mark, because the noun-phrase form is the regression this protocol replaced.

Read law before phrasing. When a live ruling exists, `fno inbox decisions <subject> --lane law --state live --json` returns the operator's own wording. Quote that wording and cite its ruling id in the block's `ruling:` field. The operator's words name the question and its authority. The plan's job is the answerer count under that question, never a second phrasing of it.

## Step 2 - Enumerate every answerer

Two halves, and a plan needs both.

### Sites: who reads or writes this answer

Sweep the repo for every place that reads or writes the answer. The pitfalls corpus rules apply in full, and each one was paid for on this exact ground:

- Never truncate or post-process a search whose zero you intend to trust.
- Never `| head` a hit list. Never `grep -c` through a wrapper. Quote every pathspec.
- Name the sweep's positive control before running it: one answerer you already know, which the command must return. A sweep that misses its control is a wrong sweep, not a clean tree.
- Never trust a hit list for a symbol you guessed rather than read. Open one answerer and read the symbol it uses before sweeping for it.

That last rule is the non-zero twin of the zero-hit trap, and it is worse. A zero invites doubt. A non-empty result reads as confirmation. The retracted lead specimen of this protocol came from grepping `SRC_PLANLESS`, an invented symbol. The search returned hits (`SRC_UNDISPATCHED`), and a partial list for a symbol that does not exist read as complete.

Record the exact sweep command in the block's `sweep:` field, and the known-before answerer in `control:`. The sweep must return the control. When it does not, the sweep is wrong, not the tree.

### Feeds: what arrives at each site you change

For each answerer the plan changes, name what supplies it, measured. Quote the expression the answerer actually evaluates, with its line. Never the constant you believe feeds it. Naming a constant is a belief about the wiring. Quoting the expression is a read of it.

Read the feed AT THE SITE, never the feed you expect the site to have.

A queue is only as real as its feed. The retracted specimen carried a real, correct measurement (31 rows, all planned) and still named the wrong queue. It measured the feed it expected instead of the feed at the site.

## Step 3 - The count is the estimate

State the answerer count in the plan's `surface:` block as `count:`. That number is the PR-count prediction. Stating it lets the operator trade one wide PR against several narrow ones today, instead of discovering the price one merged PR at a time.

## Step 4 - Dispose of every answerer

Every answerer carries one disposition, in principle 9's vocabulary. No second taxonomy:

- `dual-logic` - two hand-written implementations of one behavior. Delete a leg.
- `shared-vocabulary` - one concept spelled at many sites. The sites must agree. Porting one site retires nothing.
- `generated-artifact` - one owner, a generated copy, a freshness tripwire. Correct as built.
- `out-of-scope` - not fixed here, with a stated reason in the block.

An `out-of-scope` answerer needs only its reason. A changed answerer (`dual-logic` or `shared-vocabulary`) needs `reads`, `feed`, and `emits`, each measured.

Then state `count_after`: the number of answerers left once this plan lands. Four answerers found can become one answerer left, and `dual-logic` is the disposition that does it. A plan can leave the count where it found it, but it must say so in a number a reviewer reads beside `count`. The count trending to one is what makes the next feature on the same question cheap. A gate that only ever adds the count is a permanent tax, and a permanent tax is the rule that gets skipped.

## Why a field and a reviewer, never a paragraph

A rule that fires on a schedule decays to zero. The encounter-voting rule is injected into every session at start, is unambiguous, and drew 9 votes across 796 nodes. A rule that fires on the event it is about survives. The enumeration therefore lives in two event-fired places and nowhere else. The first is the `surface:` block the validator refuses, the planner's event. The second is one review angle asked of a fresh adversarial context, the reviewer's event, Angle F in the review lane. This file explains the protocol. It is not the gate.

## Specimens

Ten measured 2026-09-02. Each is one question with two answerers that disagree:

| The question | One answerer | The other |
|---|---|---|
| Is this row reachable? | ref truthy | ref valid |
| Was this PR reviewed? | a measurement | a hardcoded constant |
| What kind is this row? | `kind` | `type` |
| Which invocation is this? | 32-hex id | epoch-pid id |
| Is this worker done? | status live | `exited_at` set |
| How long may this read take? | inner 60s | outer 30s |
| Which nodes need dispatch? | the filter | the feed |
| Is this session alive? | one project dir | all project dirs |
| Is this pane alive? | identify probe | the process |
| How many bytes may we spend? | one ceiling | two populations |

Nine are many answerers to one question. The tenth is the inverse: one answerer conflating two questions. Both ship the same way, one facet fixed with the next facet arriving as PR two.

One of the ten is verified here rather than taken on report. "How long may this read take?" is real: `_run_json` in `cli/src/fno/king/board.py:587` defaults `timeout: int = 60`, while the stop gate that calls the board kills each external read at `STOPGATE_READ_TIMEOUT = 30s` (`crates/fno-agents/src/loopcheck.rs:11786`). The inner budget can never be spent.

The eleventh, measured the same night: "is this the operator?" has six env markers, a legacy marker, a process-tree walk and a tty as answerers. The markers outrank the tty, so a human at a keyboard was refused as agent `operator`. The fix is precedence in `resolve_owned_identity`, and it is its own node. The specimen records the shape.

## Worked example: the king board, read correctly

Question: which nodes need dispatch? Two answerers, thirty lines apart, with different feeds:

- `for node in undispatched_read.rows()` at `cli/src/fno/king/board.py:275`. Its filter drops every node without a `plan_path` (`board.py:278`). Fed by `fno backlog undispatched --json`: 31 rows, all 31 planned by construction, because `classify_planned_unclaimed` requires `status_ready` AND `plan_finalized`.
- `for node in inputs.ready.rows()` at `board.py:306`. Its filter keeps only planless nodes (`board.py:309`). Fed by `fno backlog ready --json -A`: 785 rows, 104 of them planless p0/p1 (measured 2026-09-02).

A plan that said "the blueprint queue is dead, the feed is all planned" answered the first site's question about the second site. The example teaches step 2 by showing the shape that actually got read wrong, twice. Cite the iterated expression, with its line. Read the feed at the site.

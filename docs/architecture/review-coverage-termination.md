# Review coverage termination: the pass condition, the budget, and the honest limits

The loop in one number: 32 review objects and zero approvals on a single pull request. Every round of that loop was a `COMMENTED` review. None carried a verdict anyone can act on. The pass condition was a clean round at head, and it never came. The gate this document describes exists to make that loop terminate, and this document states plainly what it does and does not close.

## The pass condition

The head is covered when the attestation chain's reviewed ranges tile `merge_base..head` AND every finding in that chain is terminal, OR the configured rounds are spent. One of three things makes a finding terminal. The three are listed below.

1. **fixed**, and a later round reviewed the fix delta,
2. **non-blocking by class**, re-derived by the gate from the finding's own primitives (never the producer's count),
3. **declined with a recorded reason**, whoever recorded it, or **waived** before this predicate runs: by a non-author `coverage-override` label, or by operator law (the next section).

Origin never gates. The author's own decline with a recorded reason is terminal exactly as a second session's is. The operator ruling is plain: a review is a review, two at most, and the recorded reason is the audit trail the event log already carries.

## The three verdict states

`fno do pr coverage-check` (and the merge guard behind it) answers one of three states:

| Exit | State | Meaning |
|------|-------|---------|
| 0 | COVERED | The chain tiles and every finding is terminal, or the configured rounds are spent. |
| 3 | REFUSED | A named conjunct failed. The refusal names what would clear it and how many rounds remain. |
| 4 | UNANSWERED | An instrument failed (head fetch, branch probe, events read). Never a verdict. |

There is no exit 5. The IMPOSSIBLE state is deleted: a spent budget is a satisfied obligation, never a locked gate. At the cap the review phase is complete, open findings stay in the PR conversation, and the PR merges on green CI. The operator lever is `config.review.max_rounds`.

## The operator-law exit

The first two exits above need a second GitHub account. The third is the one a single-account operator can run. Operator law reaches this gate through `fno.decide.current_law` and one standing subject, `review-coverage-waiver`, plus one head-scoped subject minted by the attended command:

```
fno do pr coverage-waive <pr> --reason "why this head is waived"
```

The command resolves the repository slug and the live 40-character head. It records an operator decision at `review-coverage-waiver:<owner/repo>#<pr>@<head>`, then prints `coverage waiver recorded: <owner/repo>#<pr>@<short head>`. The receipt prints only after the index write lands. Provenance decides, never a GitHub login. An attended terminal records. A harness-identified session is refused and exits nonzero. A blank reason is refused. An unreadable slug or head records nothing.

Current law resolves in three states and only one of them is authority. `single` (exactly one live law verdict on the subject) authorizes. `none` preserves the ordinary predicate. A conflict, a damaged index row, a failed read, or malformed output is UNKNOWN authority. The gate then answers UNANSWERED with the dead probe named. That is never a refusal built on a store that cannot answer, and never permission either. No caller scans the decision index, picks the newest row, or treats a failed query as `none`.

Only a row whose decision equals the affirmative value the command mints counts as authority. That value is `review coverage waived for this head`. Row existence carries no polarity: a note or a denial recorded at a waiver subject reads as no waiver, never as one. A single row with no readable decision is malformed authority and answers unknown. The command also publishes the head-pinned success status immediately after recording, so GitHub's ruleset sees the context the law already answers.

The two waiver shapes are deliberately different strengths:

- **Standing law** (`review-coverage-waiver`) waives an uncovered review for every PR. The gate narrows it: it never clears an unresolved CONFIRMED correctness or security finding, whatever the round budget says.
- **A head-scoped waiver** is explicit and strong. It covers exactly the head its subject names and clears even a hard finding at that head. It dies on the next push, because the new head's subject matches nothing.

Neither bypasses anything but review coverage. Red or pending checks, mergeability, plan fidelity, an in-flight review, and the head pin (`--match-head-commit`) all still refuse. The author-label refusal is unchanged. A `coverage-override` label applied by the PR author still reads `an author cannot override its own review gate`. The label is authorized by account identity. The command is authorized by operator provenance.

Every allowed merge prints `coverage waived: standing operator law` or `coverage waived: head-pinned operator waiver at <short head>`. A reviewed merge prints neither. The commit-status publisher posts the same receipt as success on the exact head. It replaces that marker with the computed verdict once the law no longer answers `single` for the head. `fno do pr status` clears only the review-coverage blockers under a waiver and names it as `coverage_waiver` in its payload. An unknown decision probe is its own blocker (`review_coverage_waiver_unknown`).

A waiver green can outlive its law on the GitHub side: a retraction or a conflicting ruling changes the verdict locally at once, but the posted status stays green until a publisher next runs. Two surfaces heal it, and both recompute law live: any `fno do pr status` read (a posted state that disagrees with the computed verdict triggers the republish), and every sanctioned merge attempt, each of which re-derives the verdict rather than trusting the marker. There is no law-change webhook; the residual exposure is a human merge button in the window before any publisher runs, the same staleness every commit status carries.

The Python gate and the Rust stop gate read the same law. The Rust side shells the canonical query (`fno backlog decisions <subject> --lane law --state live --json`) and parses only `current_law.status`. Both spell the standing subject identically, and the coverage-path tests pin that parity.

## The round budget

The round is the attestation law's unit (`d-608344c1`, 2026-09-03). Coverage holds when the ledger has, at the current head, one round with zero findings or two rounds with every finding disposed. A head within the interdiff carry of an attested head (`review.carry_interdiff_lines`, default 100) counts as that head. A round is keyed by HEAD. Two verdicts at one unchanged head are one round. A GitHub App review beside a local attestation at one commit is also one round: the counter is one shared set of heads across both evidence axes, not two axis counters reconciled by a max. A cap whose size depends on how a reviewer batches its output is not a cap.

`config.review.max_rounds` (default 2, at least 1) budgets those rounds across the whole life of a PR. A pass is one round like any other and refunds nothing, though it still satisfies coverage on its own terms. CI failures, lint failures and rebases are not rounds. A rebase or fix commit that moves the head by fewer than `review.carry_interdiff_lines` lines carries the prior round's attestation instead of costing a new one. At the threshold or over, or when either patch read fails, the carry dies and a fresh round is owed. A PR merges after one to three reviews and never waits for a round to come back clean.

At the cap the gate stops asking for reviews. The budget discharges the review obligation: open findings stay in the PR conversation and the PR merges on green CI, hard findings included. One review stays the floor, so an unreviewed PR is still uncovered. The old vent that filed a capped finding as a backlog node is deleted; findings live in the PR conversation where the reviewer wrote them.

## When the reviewer never answered: the lost state

A dispatch that was sent and never answered used to read as "never asked": the gate waited on a verdict that would not come. The settle sweep (`fno do review invocations settle`, also run by `fno doctor` and the stop hook) closes that. After `review.invocation_ttl_minutes` (default 15) with no answering attestation, one `review_attestation` row lands with `verdict: fail` and `output_contract: lost`, the invocation id carried in `data`. Coverage at the head reads `uncovered (lost)` - a named refusal the loop's existing uncovered-recovery can act on - instead of an absence with three explanations. The sweep is idempotent by a positive answered marker, so re-running it adds nothing.

## What this gate claims, and what it does not

**The category field is author-assigned and free-form.** The non-blocking class allowlist is a vocabulary the review producer uses to label its own findings. That allowlist is style, formatting, naming, docs, typo, nit, simplification and test-coverage. An author who controls the producer controls the label. Class-gating raises the cost of the bypass. A waived finding must be labeled, lands in the event log, and is auditable. It does not close the bypass.

**The CONFIRMED axis does enforce one thing.** A finding whose verdict is CONFIRMED blocks regardless of category. The gate re-derives this from the finding primitive. A hand-written event claiming `findings_blocking: 0` over a CONFIRMED finding is refused. What CONFIRMED means is still the producer's claim. The gate verifies the bookkeeping, not the bug.

**GitHub's refusal is per identity, not per human.** The non-author approval guard asserts the approving login differs from the PR author. GitHub refuses an author's approval of their own PR server-side. A second account with its own token can still self-approve. The guard is a belt on braces, not a proof of personhood.

**A discharged budget ships open findings.** At the cap the PR merges with findings still open. They stay in the PR conversation, in the event log, and on the coverage row (`passed_count` names how many rounds concluded pass). What the cap buys is termination. What it costs is a merge that lands with a written record of what stayed open.

**The decline exploit is auditable, not closed.** This is the plan's own statement and it belongs here, where a stranger reads it. A motivated author with producer control can manufacture a pass. What they cannot do is do it quietly. Every disposition, every category, every round is an event. The merge receipt names what cleared the gate. A gate that oversells itself is worse than no gate, because it is the doc that trains the waive.

## Where the code lives

- The producer-side classifier: `cli/src/fno/review/findings.py`.
- The gate-side re-derivation and the four states: `cli/src/fno/pr/_coverage_gate.py`.
- The freshness predicate and its interdiff-carry arm: `crates/fno-agents/src/review_freshness.rs`.
- The Rust gate twin (tiling, dispositions, rounds, human approvals): `crates/fno-agents/src/loopcheck.rs`.
- The settle sweep: `cli/src/fno/review/invocation.py`.
- The event schema (finding records, range tiling, round fields): `cli/src/fno/events/schema.yaml`.
- The review lanes this composes with: [review-lanes.md](review-lanes.md).

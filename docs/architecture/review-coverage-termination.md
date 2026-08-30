# Review coverage termination: the pass condition, the budget, and the honest limits

The loop in one number: 32 review objects and zero approvals on a single pull request. Every round of that loop was a `COMMENTED` review. None carried a verdict anyone can act on. The pass condition was a clean round at head, and it never came. The gate this document describes exists to make that loop terminate, and this document states plainly what it does and does not close.

## The pass condition

When the attestation chain's reviewed ranges tile `merge_base..head` AND every finding in that chain is terminal, the head is covered. One of four things makes a finding terminal. The four are listed below.

1. **fixed**, and a later round reviewed the fix delta,
2. **non-blocking by class**, re-derived by the gate from the finding's own primitives (never the producer's count),
3. **declined with corroboration** the author cannot mint alone (a second session's head-pinned attestation, or a non-author GitHub approval),
4. **waived** before this predicate runs: by a non-author `coverage-override` label, or by operator law (the next section).

A declined blocking finding on the author's own signature alone is not terminal. That is the whole difference between this gate and the decline exploit: receive a review, decline every finding, attest, done. The disposition pass carries its own corroboration requirement independent of `config.review.require_corroboration`, because a clean review cannot be gamed by declining and a disposition pass can.

## The four verdict states

`fno do pr coverage-check` (and the merge guard behind it) answers one of four states:

| Exit | State | Meaning |
|------|-------|---------|
| 0 | COVERED | The chain tiles and every finding is terminal. |
| 3 | REFUSED | A named conjunct failed. The refusal names what would clear it and how many rounds remain. |
| 4 | UNANSWERED | An instrument failed (head fetch, branch probe, events read). Never a verdict. |
| 5 | IMPOSSIBLE | The round budget is spent and a HARD finding is still non-terminal. Re-reviewing cannot clear it. |

A hard finding is a CONFIRMED correctness or security finding, or a truncated remainder the gate cannot inspect. Only those reach IMPOSSIBLE. Every other finding still open at the cap is FILED as a backlog node and the PR merges. That is the operator's ruling on the round cap. The class gate from the finding classifier is what makes it safe. Noise can be filed away, a confirmed correctness bug cannot.

IMPOSSIBLE names the three acts that can clear it: a non-author GitHub approval on the PR, a non-author `coverage-override` label, or the attended `fno do pr coverage-waive` command. It never asks for another review. `fno do pr merge` refuses on it with its own receipt name. `fno do pr status` renders `review_coverage_impossible` as a blocker distinct from `review_coverage_uncovered`. The git-protection hook denies a bare `gh pr merge` on exit 5 exactly as on exit 3.

## The operator-law exit

The first two exits above need a second GitHub account. The third is the one a single-account operator can run. Operator law reaches this gate through `fno.decide.current_law` and one standing subject, `review-coverage-waiver`, plus one head-scoped subject minted by the attended command:

```
fno do pr coverage-waive <pr> --reason "why this head is waived"
```

The command resolves the repository slug and the live 40-character head, records an operator decision at `review-coverage-waiver:<owner/repo>#<pr>@<head>`, and prints `coverage waiver recorded: <owner/repo>#<pr>@<short head>` only after the index write lands. Provenance decides, never a GitHub login: an attended terminal records, a harness-identified session is refused and exits nonzero. A blank reason is refused. An unreadable slug or head records nothing.

Current law resolves in three states and only one of them is authority. `single` (exactly one live law verdict on the subject) authorizes. `none` preserves the ordinary predicate. A conflict, a damaged index row, a failed read, or malformed output is UNKNOWN authority: the gate answers UNANSWERED with the dead probe named, never a refusal built on a store that could not answer, and never permission either. No caller scans the decision index, picks the newest row, or treats a failed query as `none`.

The two waiver shapes are deliberately different strengths:

- **Standing law** (`review-coverage-waiver`) waives an uncovered review for every PR, narrowed by the gate: it never clears an unresolved CONFIRMED correctness or security finding, whatever the round budget says.
- **A head-scoped waiver** is explicit and strong: it covers exactly the head its subject names, clears even a hard finding at that head, and dies on the next push, because the new head's subject matches nothing.

Neither bypasses anything but review coverage. Red or pending checks, mergeability, plan fidelity, an in-flight review, and the head pin (`--match-head-commit`) all still refuse. The author-label refusal is unchanged: a `coverage-override` label applied by the PR author still reads `an author cannot override its own review gate`, because the label is authorized by account identity while the command is authorized by operator provenance.

Every allowed merge prints either `coverage waived: standing operator law` or `coverage waived: head-pinned operator waiver at <short head>`; a reviewed merge prints neither. The commit-status publisher posts the same receipt as success on the exact head, and replaces it with the computed verdict once the law no longer answers `single` for that head. `fno do pr status` clears only the review-coverage blockers under a waiver and names it as `coverage_waiver` in its payload; an unknown decision probe is its own blocker (`review_coverage_waiver_unknown`).

The Python gate and the Rust stop gate read the same law: the Rust side shells the canonical query (`fno backlog decisions <subject> --lane law --state live --json`) and parses only `current_law.status`. Both spell the standing subject identically, and the coverage-path tests pin that parity.

## The round budget

`config.review.max_rounds` (default 2, at least 1) budgets the review rounds across the whole life of a PR. A round is one reviewed HEAD. Two verdicts at one unchanged head are one round. A cap whose size depends on how a reviewer batches its output is not a cap. A pass is one round like any other and refunds nothing, though it still satisfies coverage on its own terms. CI failures, lint failures and rebases are not rounds. A PR merges after one to three reviews and never waits for a round to come back clean.

At the cap the gate stops asking for reviews. A hard finding answers IMPOSSIBLE. Everything else is filed as a node and the merge proceeds, with the filed keys and their node ids on the merge receipt. One review stays the floor, so an unreviewed PR is still uncovered. A finding the gate cannot FILE is one it must not wave through, so a filing failure refuses instead.

## What this gate claims, and what it does not

**The category field is author-assigned and free-form.** The non-blocking class allowlist is a vocabulary the review producer uses to label its own findings. That allowlist is style, formatting, naming, docs, typo, nit, simplification and test-coverage. An author who controls the producer controls the label. Class-gating raises the cost of the bypass. A waived finding must be labeled, lands in the event log, and is auditable. It does not close the bypass.

**The CONFIRMED axis does enforce one thing.** A finding whose verdict is CONFIRMED blocks regardless of category. The gate re-derives this from the finding primitive. A hand-written event claiming `findings_blocking: 0` over a CONFIRMED finding is refused. What CONFIRMED means is still the producer's claim. The gate verifies the bookkeeping, not the bug.

**GitHub's refusal is per identity, not per human.** The non-author approval guard asserts the approving login differs from the PR author. GitHub refuses an author's approval of their own PR server-side. A second account with its own token can still self-approve. The guard is a belt on braces, not a proof of personhood.

**Filing is not fixing.** A finding filed at the cap is a finding that shipped. The node is the receipt, not the repair, and nothing schedules it. What the cap buys is termination with a written record. What it costs is a merge that lands with known open findings. Only the hard classes are exempt.

**The decline exploit is auditable, not closed.** This is the plan's own statement and it belongs here, where a stranger reads it. A motivated author with producer control can manufacture a pass. What they cannot do is do it quietly. Every disposition, every category, every round is an event. The merge receipt names what cleared the gate. A gate that oversells itself is worse than no gate, because it is the doc that trains the waive.

## Where the code lives

- The producer-side classifier: `cli/src/fno/review/findings.py`.
- The gate-side re-derivation and the four states: `cli/src/fno/pr/_coverage_gate.py`.
- The Rust gate twin (tiling, dispositions, rounds, human approvals): `crates/fno-agents/src/loopcheck.rs`.
- The event schema (finding records, range tiling, round fields): `cli/src/fno/events/schema.yaml`.
- The review lanes this composes with: [review-lanes.md](review-lanes.md).

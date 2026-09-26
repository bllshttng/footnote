# Task to verified PR: the demonstration protocol

One measured demonstration of the delivery journey on a real, non-Footnote feature. It is the evidence leg behind the operator-effort claim in [product-qualification.md](product-qualification.md); until it is measured, that claim stays "not measured".

The demonstration uses a scratch repository that is not this checkout, a fresh isolated state root, and no existing session state. The operator performs it once, records the numbers, and files them in the qualification manifest's `measurements` block with unit and source.

## The protocol

1. Prepare: pick a small real feature in a scratch repository. Create an isolated state root and install Footnote there per [getting-started.md](../getting-started.md). Start no session state in advance.
2. Carry the task: hand the feature to `/target` as one sentence and let the pipeline run. The operator gives reviews and rulings when asked, nothing more.
3. Interruption: stop a running worker mid-task (or kill the session) after it has begun implementation. Then resume from persisted state and continue to the same acceptance criteria.
4. Failed check: at the PR, let CI go red on a real defect (or break a check deliberately where the local law allows it). Let the fix loop drive the branch green. Keep the red run and the green run.
5. Recovery evidence: the interruption resume and the red-to-green transition are the demo. A wall of moving terminals is not evidence; these two events are.
6. Finish: the PR merges with review. Record the numbers below.

## The numbers

Record, with units, per accepted PR:

- Operator active minutes: only time with hands on the keyboard (commands typed, reviews given, rulings made, interruptions handled). Waiting time does not count.
- Observed spend: model and tool spend for the session, read from the ledger or provider receipts.
- Interruption resume: worked or failed, and what the operator had to do by hand.
- Failed-check recovery: red run URL, green run URL, who or what drove the fix.

File the result in `evals/fixtures/product-delivery/qualification.json` under `measurements`:

```json
"operator_active_minutes": {"unit": "minutes", "status": "measured", "source": "<demo date + PR URL>"}
```

## Honesty rules

Show the recovery, not only the win. Include unfinished runs if the demo had them. A claim grows only as far as its receipts reach: a measured Footnote number or an imported observation with provenance. Comparative and adoption claims need actual imported observations or actual customers, and none exist yet.

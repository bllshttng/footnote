# Product qualification

How a maintainer proves what the release actually does: one declared expected set, deterministic conformance, and a projection that never reports unrun work as a pass.

The release qualification matrix is declared once in `evals/fixtures/product-delivery/qualification.json`. It pins the release revision and bank revision, five scenario families, two repeats each, the declared units, and the measurement slots. Grade-only conformance and paid live trials are distinct cohort classes, and foreign products have no supported runner here: observations about them arrive only as imports with provenance.

## Run the conformance scenarios

Every command runs from the repository root with no paid model in the loop.

```bash
cd cli && uv run pytest tests/unit/test_product_qualification_manifest.py tests/unit/test_product_qualification_report.py tests/integration/test_product_delivery_journey.py -q
```

The pack journey uses the real verify, activate, evaluate, upgrade and deactivate entry points on a pack source that lives outside the core checkout:

```bash
cd cli && uv run pytest tests/integration/test_product_delivery_journey.py -q
```

Or run the scenarios as bank tasks, which writes history rows the report can fold:

```bash
fno doctor evals run --task product-delivery-journey --cohort install-first-use --repeat 2
fno doctor evals run --task product-delivery-journey --cohort viewer-detach-cold-resume --repeat 2
fno doctor evals run --task product-delivery-journey --cohort two-worker-missing-result --repeat 2
fno doctor evals run --task product-delivery-journey --cohort delivery-evidence-failure --repeat 2
```

A conformance run needs no provider because the journey task is grade-only.

## Read the report

The report verb folds the declared expected set against the history:

```bash
fno doctor evals report --qualification evals/fixtures/product-delivery/qualification.json
```

Exit 0 means every conformance scenario completed with no failed or unsupported case and no false success on record. Exit 4 means something in the matrix needs attention: a missing case, a wrong-revision row, an undeclared cohort, or an imported false success. The projection reports tested revision, expected, completed, failed, missing and unsupported counts, duration in seconds, lane fingerprints, measurements with their status, and imports with provenance. Nothing unrun ever reads as a pass.

## Claim-to-evidence table

| Claim | Reproducible command | Evidence status |
|---|---|---|
| Coherent install | `cd cli && uv run pytest tests/integration/test_install_frontdoor_journey.py -q` | measured, deterministic |
| Context retained across restart | `cd cli && uv run pytest tests/unit/test_task_context_binding.py -q` | measured, deterministic |
| Honest delivery evidence | `cargo test --manifest-path crates/fno-agents/Cargo.toml --test acceptance_evidence_journey` | measured, deterministic |
| Pack extensibility | `cd cli && uv run pytest tests/integration/test_product_delivery_journey.py -q` | measured, deterministic |
| Less operator effort per outcome | complete the demonstration in [task-to-verified-pr-demo.md](task-to-verified-pr-demo.md) | not measured |
| Superior to any external product | no supported runner exists | not measured |

Any superiority or adoption claim requires actual imported observations with tool version, fixture and date, or actual customer evidence. Until then the report says: not measured.

## Bounded live-trial protocol

Live trials are an authorized, budgeted activity. This page describes the protocol. It performs no trial.

1. The operator grants the trial: scope, budget in dollars, and the exact scenarios allowed to run with a paid worker lane.
2. Run the live-trial cohorts only: `fno doctor evals run --task product-delivery-journey --cohort operator-effort-per-outcome --repeat 2 --provider <provider> --lane <resolved lane>`
3. Keep the receipts: every row lands in the eval history with its lane fingerprint, duration and observed spend sources. A substituted lane is visible in the report.
4. Record the operator active minutes from the demonstration protocol in the manifest `measurements` block. State the unit and source. Commit the change.
5. Claims grow only as far as the receipts reach: a measured Footnote number, or an imported observation with provenance. Nothing else.

## What is deliberately absent

There is no second benchmark service, no automation of foreign tools before a supported runner exists, and no publication or outreach step. The first-run experience is documented in [getting-started.md](../getting-started.md). This guide does not rewrite onboarding. External feedback is a later observation activity, not part of this package.

# Control-plane doc colocation (advisory)

A staleness defense: when a PR changes control-plane code, the docs that describe that control plane should travel in the same diff. Docs that drift away from the code they describe rot silently. This check is the nudge that keeps them together.

It is the sibling of the [LOC ratchet](loc-ratchet.md): same control-plane path set, same merge-base diff mechanics, opposite enforcement posture. The ratchet is **blocking** (it fails CI on net growth without an exception); doc colocation is **advisory** (it warns and never blocks).

## What it does

On every PR, [`scripts/ci/control-plane-doc-colocation.sh`](../../scripts/ci/control-plane-doc-colocation.sh):

1. Computes the changed-file set as `git diff --name-only <merge-base> HEAD`, with the base resolved from `BASE_REF` (GitHub Actions sets it from `github.base_ref`) or an explicit `--base <ref>`.
2. Classifies each changed file as control-plane or not, using the **same `include:` and `exclude:` lists** the LOC ratchet reads from [`scripts/ci/loc-ratchet-manifest.yaml`](../../scripts/ci/loc-ratchet-manifest.yaml). Both sets are parsed from the manifest (with the same match semantics the ratchet uses), so test files (`**/tests/**`, `test_*`, `*_test.*`) are excluded without a hardcoded list that could drift.
3. Checks whether any changed file lives under `docs/architecture/`.
4. If control-plane code changed **and** no `docs/architecture/` file did, it emits a `::warning` annotation plus a GitHub step-summary entry listing the control-plane files. Otherwise it prints `PASS`.

The script **always exits 0**. The signal is the annotation, not a red check.

## Why one path list

Reading both the control-plane path set (`include:`) and the exclusions (`exclude:`) from `loc-ratchet-manifest.yaml` rather than duplicating them means the two checks can never disagree about what "control plane" is. Add a path to the manifest once, and both the ratchet and this nudge pick it up. The current include set:

- `hooks/`
- `scripts/lib/`
- `skills/target/scripts/verifiers/`
- `cli/src/fno/loop.py`
- `cli/src/fno/gates/`
- `cli/src/fno/gate_reality_map.yaml`
- `crates/fno-agents/src/loop*`

## Advisory by construction

Two layers keep this out of the merge gate:

- The job in [`.github/workflows/control-plane-doc-colocation.yml`](../../.github/workflows/control-plane-doc-colocation.yml) sets `continue-on-error: true`, so even if branch protection lists it the run still succeeds.
- The script exits 0 on every path, including the warning path. Any error it would otherwise raise (no base ref, missing manifest, failed diff) degrades to a soft no-op notice rather than a failure.

There is intentionally no exception ledger (unlike the ratchet's `loc-exception:` + trajectory protocol). A warning you disagree with is simply ignored; the goal is a reminder, not a checkpoint.

## Acting on a warning

When the check warns, either:

- add or update the relevant `docs/architecture/` doc in the same PR (the intended outcome), or
- ignore it when the change genuinely needs no doc update (a pure refactor, a typo fix). Nothing is blocked either way.

## Tests

[`tests/ci/test_control_plane_doc_colocation.sh`](../../tests/ci/test_control_plane_doc_colocation.sh) builds sandbox repos and covers: control-plane-without-docs warns, control-plane-with-docs passes, non-control-plane passes, test-only control-plane changes are excluded, a missing manifest is a soft no-op, and prefix-glob include entries (`sub/loop*`) match. Every case asserts exit 0.

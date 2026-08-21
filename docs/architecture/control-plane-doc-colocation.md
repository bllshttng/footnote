# Control-plane doc colocation (advisory)

A staleness defense: when a PR changes control-plane code, the docs that describe that control plane should travel in the same diff. Docs that drift away from the code they describe rot silently. This check is the nudge that keeps them together.

It is advisory: it warns and never blocks. The control-plane path set remains in the historically named `loc-ratchet-manifest.yaml` because the blast-radius router also consumes that file.

## What it does

On every PR, [`scripts/ci/control-plane-doc-colocation.sh`](../../scripts/ci/control-plane-doc-colocation.sh):

1. Computes the changed-file set as `git diff --name-only <merge-base> HEAD`, with the base resolved from `BASE_REF` (GitHub Actions sets it from `github.base_ref`) or an explicit `--base <ref>`.
2. Reads the control-plane `include:` and `exclude:` lists from [`scripts/ci/loc-ratchet-manifest.yaml`](../../scripts/ci/loc-ratchet-manifest.yaml). Manifest patterns exclude test files without a duplicated list.
3. Checks whether any changed file lives under `docs/architecture/`.
4. If control-plane code changed **and** no `docs/architecture/` file did, it emits a `::warning` annotation plus a GitHub step-summary entry listing the control-plane files. Otherwise it prints `PASS`.

The script **always exits 0**. The signal is the annotation, not a red check.

## Why one path list

The manifest supplies the control-plane paths and exclusions. The blast-radius router consumes the same lists. Add a path once, and both consumers pick it up. The current include set:

- `hooks/`
- `scripts/lib/`
- `skills/target/scripts/verifiers/`
- `cli/src/fno/loop.py`
- `cli/src/fno/gates/`
- `cli/src/fno/gate_reality_map.yaml`
- `crates/fno-agents/src/loop*`

## Advisory by construction

Two layers keep this out of the merge gate:

- The "Check control-plane doc colocation" step, in [`.github/workflows/guards.yml`](../../.github/workflows/guards.yml)'s `guards-pr` job, sets `continue-on-error: true`. The run succeeds regardless of branch protection.
- The script exits 0 on every path, including the warning path. Any error it would otherwise raise (no base ref, missing manifest, failed diff) degrades to a soft no-op notice rather than a failure.

There is intentionally no exception ledger. A warning you disagree with is simply ignored. The goal is a reminder, not a checkpoint.

## Acting on a warning

When the check warns, either:

- add or update the relevant `docs/architecture/` doc in the same PR (the intended outcome), or
- ignore it when the change genuinely needs no doc update (a pure refactor, a typo fix). Nothing is blocked either way.

## Tests

[`tests/ci/test_control_plane_doc_colocation.sh`](../../tests/ci/test_control_plane_doc_colocation.sh) builds sandbox repos and covers: control-plane-without-docs warns, control-plane-with-docs passes, non-control-plane passes, test-only control-plane changes are excluded, a missing manifest is a soft no-op, and prefix-glob include entries (`sub/loop*`) match. Every case asserts exit 0.

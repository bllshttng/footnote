# Control-plane LOC gate

## Why

An early control-plane collapse step self-assessed "net -16 lines" while executable control-plane code grew +74; the negative came entirely from markdown. Review-time discipline missed the divergence, so the discipline moves to CI.

This gate is correction 3 of the control-plane collapse design (grilled decision 11). It measures every PR's executable-LOC delta inside a checked-in path manifest and fails when the delta is positive - unless the PR declares a `loc-exception:` line in its body. The gate is permanent: it outlives the collapse initiative as the anti-regrowth immune system.

## What this gate is (and is not)

This is a **per-PR delta gate**, not a ratchet against a baseline. There is no backsliding-over-time mechanism here. An earlier revision kept a checked-in trajectory log of granted exceptions and printed a CUMULATIVE (live count - baseline) number; both were removed because nothing consumed them and every loc-exception PR appends to the end of that log, so any two such PRs conflicted by construction. What actually blocks or passes a PR is its own delta. If a backsliding-over-time measure is wanted later, it has to be built; do not look for it here.

## The three artifacts

| Artifact | Path | Role |
|---|---|---|
| Manifest | `scripts/ci/loc-ratchet-manifest.yaml` | Single source of scope: include paths, extensions, test-exclusion patterns |
| Gate script | `scripts/ci/loc-ratchet.sh` | Computes delta, enforces the decision, prints per-file breakdown |
| Workflow | `.github/workflows/guards.yml` (`guards-pr` job) | Runs the script on every PR, then the test harness, consolidated with other PR-only guards (x-b130) |

Gate tooling lives in `scripts/ci/` - outside manifest scope - so the gate never counts itself. Manifest edits are review-guarded.

## What counts

### Include paths

| Entry | Semantics |
|---|---|
| `hooks/` | All files under `hooks/` including `hooks/helpers/` |
| `scripts/lib/` | Shared shell library used by stop hook and gate audit |
| `skills/target/scripts/verifiers/` | Per-phase verifiers (9 scripts) |
| `cli/src/fno/loop.py` | Exact file match |
| `cli/src/fno/gates/` | Gate CLI and all gate modules |
| `cli/src/fno/gate_reality_map.yaml` | Canonical gate registry |
| `crates/fno-agents/src/loop*` | Forward glob for the wedge's Rust loop-check module; matches nothing today |

### Extensions counted

`.sh` `.py` `.yaml` `.yml` `.rs` - everything else (`.md`, `.js`, `.json`) is excluded by the extension whitelist regardless of path.

### Test exclusions

Path patterns applied after include + extension filter: `**/tests/**`, `**/test_*`, `**/*_test.*`. This excludes `cli/src/fno/gates/test_artifacts.py` today.

Known limitation: inline Rust `#[cfg(test)]` modules cannot be excluded at path granularity. `git diff --numstat` is line-oriented, not AST-aware; accepted.

### Delta computation

```bash
MB=$(git merge-base "origin/${BASE_REF}" HEAD)
git diff --numstat --no-renames "$MB" HEAD -- <include paths>
```

`--no-renames` is deliberate: a file moved INTO manifest scope counts as a full add; a file moved out counts as a full delete. Moving code out of scope to evade the gate is visible in review - the manifest is the trust boundary. Binary rows (`-	-` columns) are skipped. `set -euo pipefail` means any tool failure is a red check, never a silent pass.

## Decision table

| Condition | Result |
|---|---|
| delta <= 0 | PASS |
| delta > 0, PR body has a `loc-exception:` line with non-empty rationale | PASS with warning annotation |
| delta > 0, PR body is null/empty or no line matches the regex | FAIL ("no exception declared") |
| delta > 0, body line present but rationale is empty/whitespace | FAIL |
| script/parse/merge-base error | FAIL (fail-closed; never skip) |

## Declaring an exception

A single PR-body line is the whole exception. Add to the PR body (description field):

```
loc-exception: <rationale here>
```

The rationale must be non-empty on the same line. The `edited` workflow trigger re-evaluates the gate when the PR body changes, so adding this line after a red run is sufficient to re-check without a new push.

The CI failure output prints the computed delta and the per-file breakdown. There is no separate ledger to keep in sync; the exception lives entirely in the PR body, so it cannot conflict with another PR the way an append-only log does.

## Permanence and enforceability

The gate has no sunset mechanism anywhere - not in the script, the manifest, or the workflow. It enforces after the collapse initiative ends.

Enforceability requires branch protection: the gate only blocks merges once `loc-ratchet` is added as a required status check in the repository settings. This is a repo-admin action. The ship step prints the exact `gh api` command; operator action required.

The workflow runs on every PR with no path filter. A path-filtered required check leaves non-matching PRs permanently stuck on "expected" - hence no path filter.

## Workflow details

Trigger: `pull_request` with `types: [opened, synchronize, reopened, edited]`. The `edited` type is required so that adding `loc-exception:` to the PR body after a red run re-evaluates the gate without a new push.

`actions/checkout` uses `fetch-depth: 0` - merge-base computation requires full history. The default `fetch-depth: 1` produces a shallow clone where `git merge-base` fails.

PR body is passed via `env: PR_BODY: ${{ github.event.pull_request.body }}` - never interpolated inline into `run:` to avoid script injection. A null body is treated as "no exception declared".

Runtime: git + POSIX shell utilities (awk, grep, sed, wc, tr, head) only. No installs.

## Known limitations

- **Inline Rust `#[cfg(test)]` modules** are not excludable: numstat counts lines, not AST nodes. Test code inside a non-test file counts toward the delta. Accepted.

Implementation: `scripts/ci/loc-ratchet.sh`, `scripts/ci/loc-ratchet-manifest.yaml`, `.github/workflows/guards.yml` (`guards-pr` job), `tests/ci/test_loc_ratchet.sh`.

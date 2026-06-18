# Control-plane LOC ratchet

## Why

An early control-plane collapse step self-assessed "net -16 lines" while executable control-plane code grew +74; the negative came entirely from markdown. Review-time discipline missed the divergence, so the discipline moves to CI.

This gate is correction 3 of the control-plane collapse design (grilled decision 11). It measures every PR's executable-LOC delta inside a checked-in path manifest and fails when the delta is positive - unless the PR declares a `loc-exception:` in its body AND records the borrow in a checked-in trajectory file. Both factors are required; neither alone passes. The gate is permanent: it outlives the collapse initiative as the anti-regrowth immune system.

## The four artifacts

| Artifact | Path | Role |
|---|---|---|
| Manifest | `scripts/ci/loc-ratchet-manifest.yaml` | Single source of scope: include paths, extensions, test-exclusion patterns |
| Gate script | `scripts/ci/loc-ratchet.sh` | Computes delta + live cumulative, enforces the decision table, prints per-file breakdown |
| Trajectory file | `scripts/ci/loc-ratchet-trajectory.yaml` | Frozen baseline (`12778ef1`, pre-#437 main = 26,453 LOC) + append-only exceptions ledger |
| Workflow | `.github/workflows/loc-ratchet.yml` | Runs the script on every PR; also runs the test harness in a `self-test` job |

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
| delta <= 0 | PASS (cumulative summary printed) |
| delta > 0, PR body has `loc-exception:` line AND trajectory adds exactly one new entry with `delta:` == computed and non-empty `reason:` | PASS with warning annotation |
| delta > 0, new trajectory entry has an empty or missing `reason:` | FAIL |
| delta > 0, PR body is null/empty or no line matches the regex | FAIL ("no exception declared") |
| delta > 0, body line present but no new trajectory entry (or vice versa) | FAIL naming the missing factor |
| delta > 0, new trajectory entry's `delta:` != computed | FAIL printing declared vs computed |
| delta > 0, diff adds more than one new trajectory entry | FAIL (one borrow per PR; ledger stays attributable) |
| script/parse/merge-base error | FAIL (fail-closed; never skip) |

## Declaring an exception

Both steps are required. Either alone fails.

**Step 1 - PR body line.** Add a line to the PR body (description field) matching:

```
loc-exception: <rationale here>
```

The rationale must be non-empty on the same line. The `edited` workflow trigger re-evaluates the gate when the PR body changes, so adding this line after a red run is sufficient to re-check without a new push.

**Step 2 - Trajectory entry.** Append exactly one new entry under `entries:` in `scripts/ci/loc-ratchet-trajectory.yaml`:

```yaml
  - date: YYYY-MM-DD
    pr: <PR number>
    branch: <branch name>
    delta: <positive integer>
    reason: "<same rationale or expanded version>"
```

`delta:` must equal the computed delta exactly. The CI failure output states the computed number: "computed delta: +N". If a review push later changes the delta, update the single ledger line to the new number and the gate re-passes.

Known limitation: two exception PRs racing on the trajectory tail produce a git merge conflict. The second author resolves it by keeping both entries; the gate re-validates on the synchronize run.

## Trajectory file semantics

The trajectory file has two parts: a frozen baseline block and an exceptions ledger.

```yaml
baseline:
  commit: 12778ef19d61f85fda8d868a708cd6ada72c4b7c   # main immediately before the first collapse step
  executable_loc: 26453
  note: "pre-step-1 main; the collapse must end below this number"

entries:
  - date: 2026-06-04
    pr: 437
    branch: control-plane-output-validated
    delta: 74
    reason: "step 1 wedge: output_validated reads CI checks on the PR ..."
```

The `entries:` list records only PRs granted exceptions - negative or zero-delta PRs do not appear. The cumulative number is NOT the sum of entries; it is computed live on every run:

```
cumulative = (live line count of manifest files at HEAD) - baseline.executable_loc
```

This means cumulative cannot rot in a stale cache. A cumulative above zero prints a warning annotation ("initiative is still in debt"); it does not fail the gate. The per-PR delta is what blocks or passes.

The `entries:` key is required even when empty. A missing key is a parse failure - fail-closed, consistent with the gate's posture everywhere.

Re-anchor rule: if the manifest scope materially changes (include paths added or removed), add a new `baseline:` block with a dated note before the old one. The script reads only the first `baseline.executable_loc` it encounters. The old block stays for audit trail.

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
- **Duplicate trajectory entries** are rejected: if two entries carry byte-identical content, the new-entry detection (diff of entry count at HEAD vs merge-base) may not distinguish them. Vary the reason text between entries, even for the same PR.
- **Baseline tamper resistance** is review-guarded, not mechanical: a PR that silently decrements `baseline.executable_loc` would inflate the cumulative-positive threshold. The baseline block is 5 lines in a checked-in file - the change is visible in diff; the gate does not mechanize this check.

Implementation: `scripts/ci/loc-ratchet.sh`, `scripts/ci/loc-ratchet-manifest.yaml`, `scripts/ci/loc-ratchet-trajectory.yaml`, `.github/workflows/loc-ratchet.yml`, `tests/ci/test_loc_ratchet.sh`.

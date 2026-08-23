
# Auto-Merge (Opt-In)

Automatically merge a PR after external review passes. Off by default. Identical behavior across target and megawalk.

## One-Sentence Pitch Per Skill

- **target** - merge a single feature branch after review, no manual step required
- **megawalk** - walk-away multi-feature loop: each task ships and merges before the next begins

## Enable via Settings

Add to `.fno/config.toml` (project-scoped) or `~/.fno/config.toml` (global):

```yaml
config:
  auto_merge:
    enabled: true
    strategy: merge          # merge | squash | rebase (default: merge)
    delete_branch: true      # delete branch after merge (default: true)
```

Auto-merge is gated by `enabled` alone (plus the merge command's own CI-green /
external-review / stub-manifest guards). The who-may-merge gate
(`allowed_invokers` + `fno do pr merge --invoker`) was removed (x-04ab): `enabled:
true` means any surface that reaches the merge command may auto-merge, so treat
it as a project-wide opt-in, not a per-invoker allowlist.

The `strategy: merge` default preserves full commit history, which is important for
`git bisect` and forensic analysis. Squash collapses context; only use it if your
team policy requires it.

## Disable Once via CLI

Pass the positional `no-merge` modifier at invocation time to revoke merge authority for that run:

```
# target
/target M "add login page" no-merge

# megawalk
/megawalk once no-merge
```

Enabling per-run is deliberately NOT symmetric. A positional `auto-merge` grants nothing on its own. To allow merging, set `auto_merge.enabled` in config. Or spawn the run with `TARGET_AUTO_MERGE=1`. The env grant is folded into the manifest at init. Exporting it on a merge command line grants nothing.

## Resolution Order (First Match Wins)

1. `TARGET_NO_MERGE=1` - false
2. Positional `no-merge` in the invocation - false
3. `TARGET_AUTO_MERGE=1` - true
4. Local `.fno/config.toml` `config.auto_merge.enabled`
5. Global `~/.fno/config.toml` `config.auto_merge.enabled`
6. Default - false

Every refusal outranks every grant, so `no-merge` wins whenever it appears. Rung 2 sits above rung 3 on purpose: nothing in the codebase sets `TARGET_AUTO_MERGE`, so the only way it is ever set is inheritance from an ancestor shell or a spawning parent, and an inherited grant must not defeat a refusal typed into this run. The match is whole-token, so `no-merger` or a path like `plans/no-merge-notes.md` does not revoke a configured grant.

At merge and arm time both gates resolve the same posture. The gates are `fno do pr merge` and `fno-agents finalize`. Granted means live `auto_merge.enabled` or the per-run env grant. A per-run refusal is manifest `auto_merge_approved: false`. The refusal outranks every grant. `enabled` is re-read live. An operator disarm mid-flight withholds even a manifest that reads true (x-2270). One exception exists. A run granted at spawn carries `auto_merge_source: env-target-auto-merge`. That run arms on its own. Every posture refusal names its sanctioned override in its own text.

## External Review Is Mandatory Under Auto-Merge

When `auto_merge_approved: true`, `no_external` is forced to `false`
regardless of size profile or explicit `--no-external`. The Phase 8a
auto-merge gate treats `external_review_passed: skipped` as a green
light, so the combination `S + auto-merge` would otherwise merge a PR
with zero external eyes on it - a wasted PR. The override fires at
init time (`hooks/helpers/init-target-state.sh`), is logged to stderr,
and is reflected in both the live `no_external:` value and the
`skip_flags_initial.no_external:` snapshot so the drift detector
accepts it as canonical from turn one.

Implication: `/target S "feature" auto-merge` will pay the external
review wait. If you want truly fast S-mode with no review and no
merge, drop `auto-merge` and let the PR sit for manual merge.

## Why Merge-as-Default Strategy

The default merge strategy (not squash, not rebase) preserves full commit history.
This matters for `git bisect` to identify which commit introduced a regression, and for
forensic analysis of what changed and why. Squashing collapses that context into a single
commit, making post-hoc investigation harder. See PR #141 for the history behind this choice.

## Conflict Resolution

When a rebase conflict is detected during the pre-ship phase:

- A specialized `conflict-resolver` agent (Opus-class) handles conflicts automatically
- **Refuses to resolve:** migrations, secrets/credentials, lockfiles (`package-lock.json`, `yarn.lock`, `Gemfile.lock`, etc.)
- **Bails out** if the conflict spans more than 3 hunks (too risky for automated resolution)
- On refusal or bail-out: sets `status: BLOCKED` and reports which files need manual intervention
- On success: appends entry to `conflicts_resolved` in state file and continues to PR creation

## Failure Modes

The merge attempt yields one of these outcomes, written to the skill's state file:

| Outcome | Meaning | State update | Blocks promise? |
|---------|---------|-------------|----------------|
| `merged` | PR merged successfully | append PR number to `merged_prs` | No |
| `held` | Checks not green yet. Merge not attempted, retry when green | no state change | No |
| `failed` | Merge attempt failed (protected branch, permissions, etc.) | append `{pr, reason}` to `merge_failed` | No |
| `skipped` | Auto-merge disabled, or finalize already armed GitHub's queue | no state change | No |

There is no `queued` outcome (x-9d11): `fno do pr merge` executes and enforces `require_checks_pass` in-process. GitHub's native auto-merge queue is armed by `fno-agents finalize` alone, the one arming path.

A `failed` outcome does NOT block the promise or mark the session as failed. The PR was
created successfully; the merge failure is post-hoc. The user can merge manually.

## Where to See Outcomes

After a session completes, check the skill's state file:

```yaml
# .fno/target-state.md (target)
# .fno/megawalk-state.md (megawalk)
merged_prs: [42, 43]
merge_failed:
  - pr: 45
    reason: "branch protected: required status checks have not passed"
conflicts_resolved:
  - pr: 42
    files: ["src/api/users.ts"]
```

The promise line also reflects the outcome:

```
# merged
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 merged.</promise>

# held (retry when checks go green)
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 green; merge held for checks.</promise>

# failed
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 created; auto-merge failed: branch protected. Merge manually.</promise>

# skipped (auto_merge_approved: false)
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 created.</promise>
```

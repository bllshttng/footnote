
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
(`allowed_invokers` + `fno pr merge --invoker`) was removed (x-04ab): `enabled:
true` means any surface that reaches the merge command may auto-merge, so treat
it as a project-wide opt-in, not a per-invoker allowlist.

The `strategy: merge` default preserves full commit history, which is important for
`git bisect` and forensic analysis. Squash collapses context; only use it if your
team policy requires it.

## Enable Once via CLI

Pass the positional modifier at invocation time to override settings for that run:

```
# target
/target L feature.md auto-merge
/target M "add login page" no-merge

# megawalk
/megawalk auto-merge
/megawalk once no-merge
```

## Resolution Order (First Match Wins)

1. CLI positional `no-merge` - false
2. CLI positional `auto-merge` - true
3. Local `.fno/config.toml` `config.auto_merge.enabled`
4. Global `~/.fno/config.toml` `config.auto_merge.enabled`
5. Default - false

If both `auto-merge` and `no-merge` appear in the same invocation, `no-merge` wins (safer).

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

The merge attempt yields one of four outcomes, written to the skill's state file:

| Outcome | Meaning | State update | Blocks promise? |
|---------|---------|-------------|----------------|
| `merged` | PR merged successfully | append PR number to `merged_prs` | No |
| `queued` | Branch protection requires checks; merge queued | append to `merge_auto_queued` | No |
| `failed` | Merge attempt failed (protected branch, permissions, etc.) | append `{pr, reason}` to `merge_failed` | No |
| `skipped` | Auto-merge disabled (`enabled: false`) | no state change | No |

A `failed` outcome does NOT block the promise or mark the session as failed. The PR was
created successfully; the merge failure is post-hoc. The user can merge manually.

## Where to See Outcomes

After a session completes, check the skill's state file:

```yaml
# .fno/target-state.md (target)
# .fno/megawalk-state.md (megawalk)
merged_prs: [42, 43]
merge_auto_queued: [44]
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

# queued
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 queued for auto-merge.</promise>

# failed
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 created; auto-merge failed: branch protected. Merge manually.</promise>

# skipped (auto_merge_approved: false)
<promise>MISSION COMPLETE: all tasks done, tests passing, PR #42 created.</promise>
```

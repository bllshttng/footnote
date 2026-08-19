# L08: The PR lifecycle

**Medium:** Asciinema cast

**The one thing:** The PR workflow creates, checks, verifies, merges, and reconciles one pull request without treating a GitHub write as proof of completion.

## Setup state

Run the shared setup in [README.md](README.md), then enter a demo feature worktree with committed changes and green local tests. Set `PR_NUMBER` after the create step.

## 1. Read a settled control PR

```run
set -o pipefail
fno pr status 825 | jq -c '{pr, verdict, settled, green, pr_state, checks, ready, ready_blockers}'
```

```expected
{"pr":"825","verdict":"green","settled":true,"green":true,"pr_state":"MERGED","checks":{"total":14,"pass":14,"fail":0,"pending":0,"unsettled":0},"ready":true,"ready_blockers":[]}
```

## 2. Create the pull request

```run
/fno:pr create
PR_NUMBER="$(gh pr view --json number --jq .number)"
```

[capture-at-record]

## 3. Check CI and external review

```run
/fno:pr check
fno pr status "$PR_NUMBER"
```

[capture-at-record]

## 4. Rebase and verify the review gate

```run
fno pr rebase --base=origin/main
fno pr verify --kind reviews --pr-number "$PR_NUMBER" --state-file .fno/target-state.md
```

[capture-at-record]

## 5. Merge through the guarded primitive

```run
fno pr merge "$PR_NUMBER"
fno pr verify --kind merged --pr-number "$PR_NUMBER" --state-file .fno/target-state.md
```

[capture-at-record]

## 6. Run the post-merge ritual

```run
/fno:pr merged
fno pr ritual "$PR_NUMBER"
```

[capture-at-record]

## Cut list

- Keep the control verdict, created PR URL, and first live status verdict uncut.
- Compress CI waiting after the first pending verdict.
- Keep any rebase conflict and its resolution at normal speed.
- Keep the merge, merged verification, and ritual receipts visible together.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L08-the-pr-lifecycle.cast
asciinema upload L08-the-pr-lifecycle.cast
```

[capture-at-record]

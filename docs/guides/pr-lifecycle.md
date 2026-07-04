# The PR lifecycle: review, create, check, merged

`target` runs this whole arc for you. This guide is for the times you drive it by hand: you wrote the code yourself, or you're picking up a PR someone (or some agent) opened earlier. Four verbs, in order.

## 1. Review before you push: `/fno:review`

Run the internal review on your working diff before anything leaves your machine.

```
/fno:review            # sigma: the internal six-agent panel (default)
/fno:review peer       # a cross-model second opinion (e.g. Codex reviews Claude's code)
```

`sigma` fans out specialized reviewers (silent-failure hunting, test-coverage, type design, UX flows, responsive checks) over the diff and reports only findings worth acting on. `peer` routes the review to a different model than wrote the code, so you catch the things one model is systematically blind to. Fix what it surfaces, then move on.

## 2. Open the PR: `/fno:pr create`

```
/fno:pr create
```

This forks to a cheap Haiku worker that reads your commits and writes the PR title and description, then opens the PR with `gh`. It does not push code it didn't read or invent a description from thin air; the body reflects the actual diff. You get the PR URL back.

## 3. Wait for external review, then act on it: `/fno:pr check`

If you use an external review bot (configured under `config.review.external_reviewers`, e.g. a Codex or Gemini connector), `pr check` polls for its review and implements the feedback.

```
/fno:pr check
```

It waits for the bot to post, reads the inline findings, and for each blocking one either lands a fix commit or replies on the thread with a rationale. A finding counts as addressed once its thread has a non-bot reply and either a fix commit landed after it or the reply is an explicit `wontfix:`. This is the same bar `target` uses to decide a PR is done, so `pr check` and the autonomous loop agree on what "handled" means.

## 4. After it merges: `/fno:pr merged`

Merging is a human action (or `auto_merge`, if you opted in). Once the PR is merged, run the post-merge ritual:

```
/fno:pr merged          # operates on the most recent merged PR
/fno:pr merged 123      # or name the PR number
```

It reconciles the backlog (closes the node whose PR merged, even if you merged from the GitHub UI), runs the retro to capture follow-up work, reads the merged diff, and appends a dated prose section to the project's parking-lot file. It then files any triage-worthy work it found as backlog ideas.

`pr merged` needs `config.post_merge.parking_lot_path` set for this repo (the vault area is often named differently from the project, so it's never guessed). Set it with `fno setup post-merge`, or it tells you what's missing and skips the prose step.

## How this maps to `target`

When `target` ships a feature it runs review, then `pr create`, then (unless you pass `--no-external`) `pr check`, and stamps the plan. It does not run `pr merged` for you unless `config.auto_merge.enabled` is on; merging stays a deliberate step. The completion gate `target` waits on is external truth: the PR exists, CI is green, every required bot has reviewed, and no blocking inline finding is left unaddressed. The verbs above are the same machinery, exposed one step at a time.

## See also

- [Target pipeline](target.md) - the full loop and its flags
- [Cross-model review](../architecture/cross-model-review.md) - how `peer` routes to another provider
- [Auto-merge](../../skills/target/references/auto-merge.md) - opting into merge-on-green

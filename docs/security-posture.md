# Megawalk security posture

## Megawalk is for plans you trust

Megawalk is an autonomous delivery harness. When you point it at a plan, it executes the plan with your credentials, on your machine, against your repos. There is no sandbox. There is no isolation beyond standard git worktrees.

This is intentional. Sandboxing solves the wrong problem. The threat model for solo founders running plans they wrote is "my agent might make a mistake," not "my agent might be malicious." Mistake recovery is what git, atomic commits, and human review are for.

This is also a constraint when footnote is shared with others.

## The trust boundary

You should run megawalk only against plans you have read or trust the author of. Specifically:

- **Plans you wrote:** safe by definition (modulo your own mistakes).
- **Plans someone you trust wrote:** safe to the degree you trust them.
- **Plans from the footnote repository's `examples/` folder:** reviewed and safe.
- **Plans from random contributors, bug reports, gists, or untrusted sources:** read them carefully before running. Megawalk will execute them with your credentials.

We do not plan to add sandboxing in the foreseeable future. Container abstractions solve multi-tenant safety problems that solo founders do not have.

## What megawalk WILL do, given a plan

- Read and write files in the worktree.
- Run shell commands as the user invoking megawalk.
- Make Anthropic API calls with the user's credentials.
- Open pull requests on GitHub with the user's credentials.
- Push branches to remotes the user has access to.
- Edit `~/.fno/` state files.

## What megawalk WILL NOT do, by design

- Run with privileges higher than the user invoking it.
- Bypass git hooks, branch protection, or pre-commit checks.
- Skip review gates configured in the plan.
- Send your code to anyone other than the configured agent runtime.

## What megawalk CANNOT prevent, given an adversarial plan

- A plan that asks the agent to leak credentials.
- A plan that asks the agent to delete files outside the repo.
- A plan that asks the agent to push to a third-party remote.
- A plan that constructs adversarial prompts for downstream skills.

If you are evaluating an footnote plan from an untrusted source, treat it as code review, not as configuration. Read the plan content, not just the title.

## Future considerations

Two horizon items are tracked in the backlog with explicit "do not scope" tags:

- **Cost-anomaly alerts.** A real horizon need; will be planned when an incident creates concrete requirements.
- **APFS-snapshot phase rollback.** macOS-specific recovery capability; will be planned when an incident demands it.

Neither is a current capability. Do not assume either exists.

## Reporting issues

Security issues should be reported via a GitHub security advisory at https://github.com/<your-org>/footnote/security/advisories or by emailing the maintainer (see repository owner).

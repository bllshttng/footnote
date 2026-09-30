# Open PRs ready, never draft

Open pull requests **ready for review**, not draft. This holds in every repo and in background / unattended jobs.

The Claude Code background-session harness prompt tells the agent to "open a draft PR (`gh pr create --draft`)". That is a harness default, not a preference. Override it: drop `--draft` from `gh pr create`. If a background-session instruction still forces a draft, run `gh pr ready <n>` immediately after creating the PR so it lands ready.

Only open a draft when you explicitly ask for one in the moment.

Enforced, not prose: `config.pr.open_ready` (default true) drives the gh proxy, which refuses `gh pr create --draft` and `gh pr ready <n> --draft` from any Footnote-launched process, and the pr-watch sweep, which flips an open fno-bound draft PR back to ready. The one escape is an operator ruling: `fno inbox law set pr-draft:<owner/repo>#<n> "draft permitted for this pull request"` from the operator's own terminal.

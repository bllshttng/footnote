# footnote recommended rules

General-purpose claude-code rules footnote recommends. These are **opt-in**: nothing is installed unless you say yes at `fno setup` (or run the installer yourself). They land in `~/.claude/rules/` and load in every Claude Code session.

This is distinct from `.claude/rules/` inside this repo, which holds footnote's own *operational* rules (e.g. `worktrees.md`) needed to run the project. Those are not recommendations and are not installed anywhere.

Install: `fno setup` → answer yes to the recommended-rules prompt. Each rule is symlinked into `~/.claude/rules/` (copied where symlinks are unavailable), idempotently, and a real file you have hand-edited at a target path is never overwritten.

## Rules

| Rule | What it does | Harness default it overrides |
|------|--------------|------------------------------|
| [pr-ready.md](pr-ready.md) | Open PRs ready for review, not draft. | The background-session harness reflex to open draft PRs (`gh pr create --draft`). |

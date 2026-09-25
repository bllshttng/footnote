# Test value gate

footnote carries about 9,450 Rust tests and about 19,100 Python tests. The authoring gate in [skills/test-audit/SKILL.md](../../skills/test-audit/SKILL.md) applies to every test this repo adds or changes; a test that cannot answer the four questions does not get written, in TDD, in a plan task, or in a review fix.

Footnote-only mechanics, kept out of the project-neutral skill:

- Runners: `fno doctor test [paths...]` for Python (pins worktree `PYTHONPATH`, returns the real exit code; bare `pytest` in a worktree can report a false green) and `cargo test -p <crate> <filter>` for Rust.
- A campaign here is one PR per subsystem, atomic commits per lane. Merge `origin/main` into a pushed campaign branch; never rebase it.
- Proof campaigns report test counts and CI minutes before and after; the number goes in the PR body, not in chat.
- Junk-pattern specimens already seen in this repo, for the ledger: a source-grep guard that broke a PR red on a renamed identifier (not a behavior test), a UI snapshot test whose name promised one highlight color while the card rendered three, and an inventory copy test that went red because a docs table lacked one row. Each is the pattern to name when flagging its siblings.

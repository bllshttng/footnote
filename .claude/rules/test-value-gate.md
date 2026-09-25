# Test value gate

footnote carries about 9,450 Rust tests and about 19,100 Python tests. The authoring gate in [skills/test-audit/SKILL.md](../../skills/test-audit/SKILL.md) applies to every test this repo adds or changes. A test that cannot answer the four questions does not get written, in TDD, in a plan task, or in a review fix.

Footnote-only mechanics, kept out of the project-neutral skill:

- Runners: `fno doctor test [paths...]` for Python. It pins worktree `PYTHONPATH` and returns the real exit code. A bare `pytest` in a worktree can report a false green. Rust runs with `cargo test -p <crate> <filter>`.
- A campaign here is one PR per subsystem, atomic commits per lane. Merge `origin/main` into a pushed campaign branch. Never rebase it.
- Proof campaigns report test counts and CI minutes before and after. The number goes in the PR body, not in chat.
- Junk-pattern specimens already seen in this repo, for the ledger:
    - a source-grep guard that went red on a renamed identifier. It tested source text, not behavior.
    - a UI snapshot test whose name promised one highlight color while the card rendered three.
    - an inventory copy test that went red over one missing docs-table row.
- Flag siblings by naming the pattern they share.

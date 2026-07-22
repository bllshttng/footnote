# Fix in the project, never memory-only

footnote is open source: users inherit the code, the docs, and the runtime behavior - never your private agent memory. A load-bearing fact kept only in memory is a fix you mailed to yourself; it ships to nobody.

**The trigger:** the moment you reach for memory to record a workaround, a non-obvious invariant, a behavior gap, a trap, or a "next time do X" - stop and write a project artifact instead. If a teammate would hit the same wall, it belongs in the project.

**Where it lands (most durable first):**

1. The fix itself - a guard, gate, corrected default, or verb that makes the trap impossible.
2. Self-teaching runtime text - a refusal message, receipt line, or `--help` string (cannot drift from behavior).
3. A doc (`docs/`) or rule (`.claude/rules/`), plus an `AGENTS.md` pointer if load-bearing enough.
4. A test that fails loudly when the invariant breaks.
5. A filed node whose details name the concrete fix path (`fno backlog idea` - the verb, the file, the gate).

**What memory IS for:** user preferences, session continuity, who's-who. The discriminator: would a stranger cloning the repo need this? Yes -> project artifact. Only this user/run -> memory. When unsure, write the project artifact.

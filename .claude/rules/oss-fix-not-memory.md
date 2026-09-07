# Fix in the project, never memory-only

footnote is open source: users inherit the code, the docs, and the runtime behavior - never your private agent memory. A load-bearing fact kept only in memory is a fix you mailed to yourself. It ships to nobody.

**The trigger:** you reach for memory to record a workaround, a non-obvious invariant, a behavior gap, a trap, or a "next time do X". Stop at that moment and write a project artifact instead. If a teammate can hit the same wall, it belongs in the project.

**Reach test:** every project that installs this plugin, or only this team? Universal goes to tier 1, the machine. Team-specific starts lower.

**Config before rule:** for team-specific work, a knob beats prose. A knob is checkable and has a default. A rule is a workaround.

**Where it lands (most durable first):**

1. The fix itself - a guard, gate, corrected default, or verb that makes the trap impossible.
2. Self-teaching runtime text - a refusal message, receipt line, or `--help` string. It cannot drift from behavior. It drifts from POLICY freely. A ruling that retires a command leaves every string still teaching it, and `scripts/ci/check-retired-command-strings.sh` closes that gap.
3. A doc (`docs/`) or rule (`.claude/rules/`). If it is load-bearing enough, add an `AGENTS.md` pointer.
4. A test. When the invariant breaks, it fails loudly.
5. A filed node whose details name the concrete fix path (`fno backlog idea` - the verb, the file, the gate).

**What memory IS for:** user preferences, session continuity, who's-who. The discriminator: does a stranger cloning the repo need this? Yes -> project artifact. Only this user/run -> memory. When unsure, write the project artifact.

# Code Review Report Template

```markdown
## Code Review Report

**Change Type Detected:** [backend/frontend/full-stack/docs-only/other]
**Files Changed:** [count]
**Commits Reviewed:** [count]

---

### Agents Run
| Agent | Status | Summary |
|-------|--------|---------|
| silent-failure-hunter | [ran] | [findings summary] |
| code-reviewer | [ran] | [findings summary] |
| [conditional agents...] | [ran] | [findings summary] |

### Agents Skipped (not applicable)
| Agent | Reason |
|-------|--------|
| ux-flow-tester | No frontend changes detected |
| multi-device-checker | No frontend changes detected |
| type-design-analyzer | No backend changes detected |
| integration-test-analyzer | No backend changes detected |

### Automated Checks
| Check | Status |
|-------|--------|
| TypeScript | Pass/Fail |
| Lint | Pass/Fail |
| Journey Tests | Pass/Fail/Skipped |
| Integration Tests | Pass/Fail/Skipped |
| Build | Pass/Fail |

### Goal Relevance

_If project goals are defined in config.toml:_

| Goal | Relevance |
|------|-----------|
| G2: Do-target autonomy | **Primary** — changes directly advance this goal |
| G4: Cost observability | **Secondary** — changes support infrastructure for this goal |
| G1, G3, G5 | Not related to these changes |

_No scope creep detected._ / _⚠️ Changes touch areas outside stated goals — verify intent._

_If no project goals defined: "No project goals defined in config.toml — skipping relevance check"_

---

### Critical Issues (Must Fix)
1. [Description] — `agent: <agent-name>` `provider: <provider_id>` `file: <path>` `line: <N>`

### High Priority Issues
1. [Description] — `agent: <agent-name>` `provider: <provider_id>` `file: <path>` `line: <N>`

### Recommendations
1. [Description] — `agent: <agent-name>` `provider: <provider_id>`

_`provider` is the id from `config.providers.records[]` for the agent that produced this
finding. Forensics-only — does not affect verdict or severity._

---

### Verdict
- [ ] **Ready to merge** - All tests pass, coverage adequate
- [ ] **Needs work** - See critical issues above
- [ ] **RECOMMEND RESTART** - Fix-in-place is worse than re-derivation (see contract below)

### Notes
- [Any special observations, e.g., "Shell scripts detected - manual bash review recommended"]
```

## Terminal recommendation: RECOMMEND RESTART

A blocking review is fix-in-place by default: the operator addresses the findings on the same branch. `RECOMMEND RESTART` is the one verdict that says *don't* fix in place - discard this attempt and re-derive from a fresh node. It exists because a builder never discards its own work (the generator is proud of its output), so the order to restart has to come from the reviewer, not the session under review.

**Legal only when** the panel judges re-derivation cheaper than patching: wrong architecture, a cascading design error the findings all descend from, or patch-on-patch accumulation where each fix spawns the next. Severity alone is NOT a trigger - a pile of P1s that are each fixable in place is a fix round, not a restart.

A `RECOMMEND RESTART` verdict MUST carry both:

1. **Why fix-in-place fails** - not a severity count, the specific reason re-derivation beats patching here.
2. **A lessons block** - what this attempt learned and what the successor must avoid, in prose the successor's node `details` can carry verbatim.

It must also name the honor sequence by explicit path: `skills/target/references/failure-recovery.md`, "Reviewer-ordered restart" section.

**Malformed degrades to blocking.** A `RECOMMEND RESTART` missing its rationale or its lessons block is treated as a normal `Needs work` verdict (degrade, never guess an incomplete restart into an executed one). The operator side never honors a malformed recommendation.

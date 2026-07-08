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

### Notes
- [Any special observations, e.g., "Shell scripts detected - manual bash review recommended"]
```

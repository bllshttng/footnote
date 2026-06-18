---
name: audit
description: "Multi-perspective feature completeness analysis and planning loop. Use when: 'audit feature', 'what's missing', 'feature completeness', 'gap analysis', 'discover features to build', 'plan all features'."
argument-hint: "TOPIC [--max-iterations N] [--output-dir PATH] [--perspectives LIST]"
metadata:
  internal: true
hooks:
  PreToolUse:
    - matcher: ".*"
      once: true
      hooks:
        - type: command
          command: "SESSION_TYPE=audit ${CLAUDE_PLUGIN_ROOT}/hooks/helpers/init-session-state.sh"
---

# Audit Skill

Analyze feature completeness from multiple perspectives, identify gaps, and create phased plan folders. Runs as a loop until ALL features are planned.

## Purpose

Force comprehensive feature discovery by analyzing from every angle until nothing is missed. Don't stop after surface-level analysis.

## Setup

Run the setup script to initialize the audit loop:

```bash
"${CLAUDE_PLUGIN_ROOT}/scripts/setup-audit.sh" $ARGUMENTS
```

This creates:
- `.fno/audit-loop.local.md` — Loop config with completion promise
- `.fno/audit-progress.txt` — Iteration-persistent progress tracker

If the setup script sets a completion promise, read it and follow its instructions.

## Options

| Option | Description |
|--------|-------------|
| `--max-iterations N` | Stop after N iterations (default: 20) |
| `--output-dir PATH` | Where to create plan folders (default: from `.claude/settings.json` `plansDirectory`, or `config.plans.full_path`) |
| `--perspectives LIST` | Comma-separated: ux,pm,po,eng (default: all) |

## Required Skills

Load these skills during audit:
- `/think` — For brainstorming and design exploration
- `/blueprint` — For creating plan folders
- `/tdd` — For writing testable stories (BDD criteria in references/)
- `/setup` — For cross-project awareness (workspace config in references/)

## Process

### 1. Load Context (Every Iteration)

Each iteration is a FRESH context. Memory persists ONLY via:
- **Progress file** — `.fno/audit-progress.txt` (READ THIS FIRST!)
- **Created plans** — Plan folders you've already created
- **Git history** — Commits from previous iterations

### 2. Analyze Current State

Scan codebase and existing plans to build completeness matrix:

```markdown
## Current State Summary

| Area | Status | Completeness | Evidence |
|------|--------|--------------|----------|
| QR Sign-in Flow | ✅ Complete | 95% | src/routes/sign-in/, tests pass |
| Child Roster | ✅ Complete | 90% | CRUD works, missing bulk import |
| Ratio Monitoring | ⚠️ Partial | 80% | Real-time works, no forecasting |
| Notifications | ❌ Missing | 0% | No SMS delivery, no alerts |
```

**How to scan:**
1. Read existing plan INDEX files
2. Search codebase for implemented features
3. Run tests to verify what works
4. Check for TODO comments and incomplete features

### 3. Multi-Perspective Gap Analysis

Analyze from FOUR perspectives. Don't skip any.

#### UX Research Perspective

```markdown
## User Journey Gaps

| Journey | Gap | Impact | Priority |
|---------|-----|--------|----------|
| Parent First-Time | No onboarding flow | High friction | P1 |
| Staff Discovery | No setup guide | Confusion | P1 |
| Multi-Child Parent | One-at-a-time sign-in | Slow | P2 |

## Missing Edge Cases
- Custody changes: No same-day revocation
- Emergency contacts: Can't mark "no pickup"
- Late pickup: No alerts

## Accessibility Gaps
- Signature canvas needs keyboard alternative
- Color-only status indicators
```

#### Product Owner Perspective (INVEST Stories)

```markdown
## Must-Have (P1) - Blockers

**US-1: SMS Delivery**
As a parent, I need to receive OTP via SMS
so that I can verify my phone.

Acceptance Criteria:
- [ ] Twilio sends real SMS
- [ ] Handles delivery failures
- [ ] Rate limits (5/hour/phone)

## Should-Have (P2) - Adoption

**US-4: Multi-Child Sign-in**
As a parent with multiple children, I need to sign them together
so that drop-off is faster.

## Could-Have (P3) - Scale

**US-7: PWA**
As a parent, I want an app instead of QR each time...
```

#### Product Manager Perspective

```markdown
## MVP vs Full Feature Matrix

| Capability | MVP (Current) | Full Feature | Status |
|------------|---------------|--------------|--------|
| OTP | Dev only | SMS delivery | Gap |
| Bulk Import | Manual | CSV import | Gap |
| Notifications | None | Push/SMS | Gap |

## Success Metrics to Implement
| Metric | Why | Implementation |
|--------|-----|----------------|
| Sign-in Time | UX quality | event_time - session_start |
| Override Rate | Process health | overrides / total_events |
```

#### Engineering Perspective

```markdown
## Technical Debt
- OTP service is mock-only
- No rate limiting on public endpoints
- Signature canvas not optimized for mobile

## Performance Gaps
- Ratio calculation runs on every render
- No caching on roster queries

## Security Gaps
- Phone validation endpoint needs rate limit
- No CSRF on form submissions
```

#### Integration Coherence Perspective (The Wiring Inspector)

Don't ask "is each feature complete?" — ask "do the features work TOGETHER?"

```markdown
## User Journey Wiring

For each major user journey, trace the full path:

| Journey | Path | Break Point | Status |
|---------|------|-------------|--------|
| Parent signs in child | QR scan → verify → record event → update ratio | None | ✅ Connected |
| Staff views ratio | Dashboard → fetch ratios → calculate → display | ratio calc uses mock data | ⚠️ Stubbed |
| Admin runs report | Click export → generate PDF → download | PDF generator not wired | ❌ Orphaned |

## Orphaned Features (Built but not reachable)
- Components that exist but have no route/navigation to them
- API endpoints that exist but no UI calls them
- Database tables with no read/write operations

## Stub Dependencies (Wired to placeholders)
- Functions that return hardcoded/mock data
- External service integrations using dev-only endpoints
- Feature flags permanently set to false

## Partial Wiring (Half-connected)
- Frontend calls API that returns TODO response
- Backend writes to table that frontend never reads
- Event emitted but no listener registered
```

**Verification method:** For each journey, the auditor should:
1. Start from the UI entry point (or API if headless)
2. Trace through actual code (grep for function calls, imports, routes)
3. Mark each link as connected, stubbed, or broken
4. If a link is broken, note what task would fix it

### 3b. Goal Progress Cross-Reference (MANDATORY if settings.yaml has goals)

After feature discovery, cross-reference against project goals:

1. Read `project.goals` from settings.yaml (`.fno/settings.yaml` or `~/.fno/settings.yaml`)
2. Read `~/.fno/ledger.json` entries (if exists)
3. For each goal, find tasks whose `branch` or `summary` relates to the goal
4. Produce a progress table:

```markdown
## Goal Progress

| Goal | Status | Tasks | Total Cost | Notes |
|------|--------|-------|------------|-------|
| G1: Open source | not_started | 0 | $0 | ⚠️ No work started |
| G2: Do-target autonomy | in_progress | 3 | $284.37 | Active development |
| G3: Quality gates | in_progress | 2 | $62.66 | |
| G4: Cost observability | in_progress | 1 | $420.86 | |
| G5: Subagent orchestration | in_progress | 1 | $31.70 | |

Recommendation: G1 has no work yet. Consider prioritizing if open source is a near-term objective.
```

This mapping is approximate — use task summaries and branches to infer goal alignment. Explicit goal tags in ledger.json may be added in a future iteration.

If ledger.json or settings.yaml doesn't exist, note: "Goal progress unavailable — no ledger.json or settings.yaml found"

If a goal has status `not_started` and zero ledger.json entries, flag it: "⚠️ No work started — consider prioritizing"

### 4. Prioritize into Phases

Group by deployment readiness:

```markdown
## Phase 1: Go-Live (Blockers)
Must complete before real users:
- SMS delivery (can't receive OTPs)
- Rate limiting (security)
- Staff setup guide (operational)

## Phase 2: Adoption (Enablers)
Reduce friction, increase usage:
- Multi-child sign-in
- Bulk CSV import
- Expected absence tracking

## Phase 3: Scale (Enhancements)
Advanced features:
- PWA for parents
- Photo verification
- Ratio forecasting
```

### 5. Create Plan Folders

For each phase, use `/blueprint` skill to create:

```
{plans_path}/phase-1-go-live/
├── 00-INDEX.md          # Phase overview, dependencies
├── 01-sms-delivery.md   # Twilio integration tasks
├── 02-rate-limiting.md  # Security tasks
└── 03-staff-wizard.md   # Onboarding tasks
```

**Link to single Linear epic** if Linear is configured (`config.linear.enabled`):
```yaml
linear: {TEAM}-XXX  # "Phase 1: Go-Live"
```

### 6. Loop Check

After creating plans, ask:

```markdown
## Remaining Gaps Check

Have I covered:
- [ ] All P1 blockers?
- [ ] All user journey gaps?
- [ ] All edge cases mentioned?
- [ ] All accessibility issues?
- [ ] All technical debt items?
- [ ] All security concerns?
- [ ] All critical user journeys traced end-to-end?
- [ ] All orphaned features identified?
- [ ] All stub dependencies documented?

If ANY unchecked → Continue planning
If ALL checked → Loop complete
```

## Progress File Format

```markdown
# .fno/audit-progress.txt

topic: QR code sign-in feature completeness
started: 2026-01-23T10:00

## Analysis Complete
- [x] Current state scan
- [x] UX perspective
- [x] PM perspective
- [x] PO perspective
- [ ] Engineering perspective

## Plans Created
- phase-1-go-live/: 4 features (SMS, rate-limit, wizard, welcome)
- phase-2-adoption/: 3 features (multi-child, bulk-import, absences)

## Remaining Gaps
- Late pickup alerts (P2)
- Ratio forecasting (P3)
- PWA (P3)

## Next Actions
1. Create phase-3-scale/ folder
2. Document remaining P3 features
```

## Output Artifacts

After audit loop completes:

1. **Phase folders** in output directory
2. **Linear tickets** for each phase
3. **Progress file** with analysis summary
4. **Completeness matrix** in progress file

## Completion

If a completion promise is set in `.fno/audit-loop.local.md`, you may ONLY output it when ALL features for the topic are documented in plan folders. Do not stop after one pass — keep asking "what else is missing?" until truly feature-complete.

## Key Principles

- **Don't stop early** — Keep asking "what else?"
- **All perspectives** — UX, PO, PM, Eng, Integration Coherence
- **Trace journeys, not features** — A feature isn't "done" if users can't reach it
- **Check the wiring** — Every component must be connected to something upstream AND downstream
- **Concrete plans** — Not just lists, actual plan folders
- **Testable stories** — Every feature gets acceptance criteria
- **Phased output** — P1/P2/P3 priority grouping
- **Linked to Linear** — Every plan has a ticket (if Linear configured)

## Red Flags

**Never:**
- Stop after one perspective
- Create lists without plan folders
- Skip edge cases and accessibility
- Assume "good enough"
- Output completion promise with remaining gaps

**Always:**
- Scan codebase before analyzing
- Cover all four perspectives
- Create actual plan folders (not just notes)
- Link plans to Linear tickets (if configured)
- Update progress file after each iteration

## Session Cost Tracking (AUTO — enforced by stop hook)

Cost is automatically registered by the stop hook when the session exits. The stop hook scans the transcript for `fno:audit` Skill tool invocations, calculates cost via `session-cost.py`, and appends to `ledger.json` via `register-task.py`. No manual action needed.

This is non-blocking — if it fails, the audit is complete regardless.

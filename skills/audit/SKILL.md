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

Analyze feature completeness from the configured perspectives, identify gaps, and record them in a bounded evidence artifact. Plan documents are written only on an authorized run. The loop is bounded by the audit's own named unresolved questions, not by "nothing is missed".

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
| `--output-dir PATH` | Where to write plan documents when the run is authorized to produce plans (default: from `.claude/settings.json` `plansDirectory`, or `config.plans.full_path`) |
| `--perspectives LIST` | Comma-separated lens subset; the resolved list is the authority for "do not skip any" (default: all five - ux, pm, po, eng, integration) |

## Skills, loaded for their own act

- `/think` — load for structured design exploration a lens needs
- `/blueprint` — load for plan production on an authorized run
- `/tdd` — load for acceptance-criteria writing on planned features
- `/setup` — load for cross-project workspace context

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
1. Read existing plan documents in the output directory
2. Search codebase for implemented features
3. Run the project's verification command once to ground the matrix - not once per lens
4. Check for TODO comments and incomplete features

### 3. Multi-Perspective Gap Analysis

Analyze from every lens in the resolved `--perspectives` set (default: all five, including Integration Coherence). Do not skip a lens the run resolved.

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

### 3b. Goal Progress Cross-Reference (MANDATORY if config.toml has goals)

After feature discovery, cross-reference against project goals:

1. Read `project.goals` from config.toml (`.fno/config.toml` or `~/.fno/config.toml`)
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

If ledger.json or config.toml doesn't exist, note: "Goal progress unavailable — no ledger.json or config.toml found"

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

### 5. Plan Documents (authorized runs only)

A plain audit's deliverable is the evidence artifact: the completeness matrix, the gap lists, and the progress file. On an authorized run that also asked for plans, use `/blueprint` per feature. The canonical blueprint writes ONE Markdown document per feature (single doc, locked frontmatter), never a phase folder of INDEX plus numbered files:

```
{plans_path}/sms-delivery.md      # one blueprint doc per feature
{plans_path}/rate-limiting.md
{plans_path}/staff-wizard.md
```

On an operator request to turn gaps into work, file each audited gap as a backlog node (`fno backlog idea`). The audit itself does not create plans or nodes unless asked.

### 6. Loop Check

After each analysis pass, ask whether every gap named so far is ANSWERED, FILED (node or plan), or explicitly PARKED:

```markdown
## Remaining Gaps Check

- [ ] Every P1 blocker found is answered, filed, or parked
- [ ] Every user journey gap found is answered, filed, or parked
- [ ] Every edge case and accessibility gap found is answered, filed, or parked
- [ ] Each named unresolved question has an owner or a parking note

If ANY unchecked → Continue (bounded by --max-iterations)
If ALL checked → Loop complete
```

"Cover everything that exists" is not a finish line. The named questions the audit raised are.

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

After the audit loop completes:

1. **Progress file** with analysis summary (`.fno/audit-progress.txt`)
2. **Completeness matrix** in the progress file - the bounded evidence artifact
3. **Plan documents** in the output directory on an authorized run (one blueprint doc per feature)

## Completion

With a completion promise set in `.fno/audit-loop.local.md`, output it only after every question the audit named is answered, filed, or parked. Keep the output inside the iteration bound. Do not stop after one pass. Do not keep looping past the bound either. At the bound, report the named questions that remain.

## Key Principles

- **Do not stop early** — Keep asking "what else?" within the iteration bound
- **All resolved perspectives** — the `--perspectives` set, five by default (UX, PO, PM, Eng, Integration Coherence)
- **Trace journeys, not features** — A feature isn't "done" if users can't reach it
- **Check the wiring** — Every component must be connected to something upstream AND downstream
- **Grounded output** — the evidence artifact first, plan documents only on an authorized run
- **Testable stories** — Every planned feature gets acceptance criteria
- **Priority grouping** — P1/P2/P3

## Red Flags

**Never:**
- Stop after one perspective
- Create plans or nodes the run was not authorized to create
- Skip edge cases and accessibility
- Assume "good enough"
- Output completion promise with named questions still unowned

**Always:**
- Scan codebase before analyzing
- Cover every lens in the resolved set
- Record findings in the progress file as you go
- Update progress file after each iteration

## Session Cost Tracking (AUTO — enforced by stop hook)

Cost is automatically registered by the stop hook when the session exits. The stop hook scans the transcript for `fno:audit` Skill tool invocations, calculates cost via `session-cost.py`, and appends to `ledger.json` via `register-task.py`. No manual action needed.

This is non-blocking — if it fails, the audit is complete regardless.

## Known Limitations and Deferred Work

- Audits cannot prove behavior owned by unavailable services. See [LIMITATIONS.md](LIMITATIONS.md).

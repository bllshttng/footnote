# Runbook Template

---
created: YYYY-MM-DDTHH:MM
last_tested: YYYY-MM-DD
owner: [team or person]
---

# [Operation] Runbook

## Overview

**What**: [What this runbook covers]
**When**: [When to use this runbook]
**Time**: [Expected duration]
**Risk**: [low | medium | high]

## Prerequisites

- [ ] [Access/permission 1]
- [ ] [Tool/credential 2]
- [ ] [Notification sent]

## Procedure

### Step 1: [Action]

```bash
# Command to run
```

**Expected output**:
```
[What you should see]
```

**If you see an error**: [What to do]

### Step 2: [Next Action]

[Continue...]

## Verification

- [ ] [Check 1 passes]
- [ ] [Check 2 passes]
- [ ] [Metrics look normal]

## Rollback

If something goes wrong:

### Step 1: [Undo action]
```bash
# Rollback command
```

### Step 2: [Verify rollback]
[How to confirm rollback worked]

## Troubleshooting

### "[Error message or symptom]"
**Cause**: [Why this happens]
**Fix**: [How to resolve]

### "[Another error]"
**Cause**: [...]
**Fix**: [...]

## Post-Operation

- [ ] Update monitoring/dashboards
- [ ] Notify stakeholders
- [ ] Document any issues encountered

## Contacts

| Role | Name | Contact |
|------|------|---------|
| Primary | [Name] | [Slack/phone] |
| Escalation | [Name] | [Slack/phone] |

Save to: `internal/{project}/operations/{operation}-runbook.md`

# Deployment Guide Template

---
created: YYYY-MM-DDTHH:MM
environment: [production | staging | all]
---

# [Service] Deployment Guide

## Overview

**Service**: [Name]
**Repository**: [URL]
**Deploy frequency**: [How often]
**Deploy window**: [When deployments happen]

## Prerequisites

- [ ] All tests passing on main
- [ ] PR approved and merged
- [ ] [Other requirements]

## Deployment Steps

### 1. Pre-Deploy Checks

```bash
# Verify build status
gh run list --workflow=ci

# Check current version
curl https://api.example.com/health
```

### 2. Deploy

```bash
# Deploy command
[command]
```

### 3. Verify Deployment

```bash
# Health check
curl https://api.example.com/health

# Smoke test
[command]
```

**Expected**: [What success looks like]

### 4. Monitor

Watch for 15 minutes:
- [ ] Error rates stable
- [ ] Latency normal
- [ ] No user complaints

## Rollback

If issues detected:

```bash
# Rollback command
[command]
```

## Environment Variables

| Variable | Description | Where Set |
|----------|-------------|-----------|
| `VAR_NAME` | [Purpose] | [Vercel/Supabase/etc] |

## Secrets

| Secret | Purpose | Rotation |
|--------|---------|----------|
| `API_KEY` | [What it's for] | [When rotated] |

## Contacts

[Who to contact for issues]

Save to: `internal/{project}/deployment/{service}-deploy.md`

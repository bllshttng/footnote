# API Contract Template

---
created: YYYY-MM-DDTHH:MM
version: 1.0.0
status: draft | stable | deprecated
---

# [Service/Feature] API Contract

## Overview

[What this API does, who uses it]

## Base URL

```
Production: https://api.example.com/v1
Staging: https://api-staging.example.com/v1
```

## Authentication

[How to authenticate - JWT, API key, etc.]

## Endpoints

### `POST /resource`

**Description**: [What it does]

**Request**:
```typescript
interface CreateResourceRequest {
  name: string
  // ... fields with descriptions
}
```

**Response** (200):
```typescript
interface CreateResourceResponse {
  id: string
  // ... fields
}
```

**Errors**:
| Code | Message | Cause |
|------|---------|-------|
| 400 | Invalid input | [When this happens] |
| 401 | Unauthorized | [When this happens] |
| 409 | Already exists | [When this happens] |

### `GET /resource/:id`

[Continue for each endpoint...]

## Webhooks

[If applicable - what events, payload format]

## Rate Limits

[Limits per endpoint or global]

## Versioning

[How versions work, deprecation policy]

## Changelog

| Version | Date | Changes |
|---------|------|---------|
| 1.0.0 | YYYY-MM-DD | Initial release |

Save to: `internal/{project}/specs/api-{service}.md`

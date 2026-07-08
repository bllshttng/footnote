# Abilities Docs

Create and maintain internal technical documentation: architecture docs, specs, runbooks, and deployment guides.

**This skill is for developer/internal docs only.** For end-user how-to guides, use `/how-to` instead.

## Reference Materials

Load the appropriate reference based on doc type:

| Reference | Load When | Content |
|-----------|-----------|---------|
| [references/architecture-template.md](references/architecture-template.md) | Creating architecture docs, system design, component graphs | Mermaid-first template with component tables, data flows |
| [references/api-contract-template.md](references/api-contract-template.md) | Documenting API endpoints, schemas, versioning | OpenAPI-style endpoint documentation |
| [references/spec-template.md](references/spec-template.md) | Writing feature requirements, acceptance criteria | RFC-format technical specification |
| [references/runbook-template.md](references/runbook-template.md) | Creating operational procedures, incident response | Step-by-step operational runbook |
| [references/deployment-template.md](references/deployment-template.md) | Writing deploy/rollback/verify procedures | Deployment guide with rollback plan |

## Step 0: Resolve Documentation Root (MANDATORY)

Read documentation paths from config.toml:
1. Check `.fno/config.toml` -> `config.docs.architecture_path`
2. Fall back to `~/.fno/config.toml` -> `config.docs.architecture_path`
3. Default: `docs/architecture`

```bash
# Verify docs directory exists
ls {architecture_path}/
```

## Documentation Structure

```
{docs_root}/
  architecture/         -> Architecture docs (component graphs, data flows, Mermaid diagrams)
  specs/                -> Technical specifications
  operations/           -> Runbooks, incident response
  deployment/           -> Deployment guides
```

## Default Output Paths

| Doc Type | Path | Example |
|----------|------|---------|
| Architecture | `{config.docs.architecture_path}/{feature}.md` | `docs/architecture/chat-ux.md` |
| Spec | `{config.docs.architecture_path}/specs/{feature}-spec.md` | `docs/architecture/specs/auth-spec.md` |
| Runbook | `{config.docs.architecture_path}/operations/{op}-runbook.md` | `docs/architecture/operations/deploy-runbook.md` |

**No ADRs by default.** Write architecture docs with Mermaid diagrams, component tables, data flow descriptions. Only use ADR format if the user explicitly requests it.

## Process

### 1. Identify Location

```bash
# Check what exists
ls {architecture_path}/
```

### 2. Choose Doc Type & Load Template

| Type | Purpose | Template |
|------|---------|----------|
| **Architecture** | System design, component relationships | Load [references/architecture-template.md](references/architecture-template.md) |
| **API Contract** | Endpoints, schemas, versioning | Load [references/api-contract-template.md](references/api-contract-template.md) |
| **Spec** | Feature requirements, acceptance criteria | Load [references/spec-template.md](references/spec-template.md) |
| **Runbook** | Step-by-step operational procedures | Load [references/runbook-template.md](references/runbook-template.md) |
| **Deployment** | How to deploy, rollback, verify | Load [references/deployment-template.md](references/deployment-template.md) |
| **Plan** | Implementation approach (from `/blueprint`) | Task-based (see `/blueprint` skill) |

### 3. Write Using Loaded Template

Load the appropriate reference file, then fill in the template with project-specific content.

## Frontmatter Standards

Every doc should include:

```yaml
---
created: YYYY-MM-DDTHH:MM
updated: YYYY-MM-DDTHH:MM   # Add when modified
status: draft | review | approved | deprecated
---
```

## Quality Checklist

Before finalizing:
- [ ] Saved in correct `{config.docs.architecture_path}/{doc-type}/` location
- [ ] Has proper frontmatter with dates
- [ ] Commands are copy-pasteable (no placeholders without explanation)
- [ ] Verification steps included
- [ ] Rollback/recovery documented
- [ ] Contacts/escalation listed (for ops docs)

## Key Principles

- **Location matters** - Follow the folder structure
- **Keep current** - Update `updated:` date on changes
- **Runnable commands** - No unexplained placeholders
- **Verify success** - Always include "how to know it worked"
- **Plan for failure** - Rollback and troubleshooting sections

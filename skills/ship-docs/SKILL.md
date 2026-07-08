---
name: ship-docs
description: "Generate and maintain project documentation - architecture docs, how-to guides, API references. Use when: 'document this', 'architecture docs', 'API contract', 'runbook', 'user guide', 'how-to', shipping documentation for a feature."
---

# Ship Docs

Generate and maintain project documentation: architecture docs, how-to guides, API references, runbooks.

## Two Types of Documentation

| Type | Audience | Reference |
|------|----------|-----------|
| **Architecture docs** | Developers, internal team | [references/architecture-docs.md](references/architecture-docs.md) |
| **How-to guides** | End users (by role) | [references/how-to-guides.md](references/how-to-guides.md) |

## Process

### 1. Determine What's Needed

Check the feature scope:
- **Architecture docs**: New systems, APIs, data flows, deployment changes
- **How-to guides**: User-facing features, workflow changes, new capabilities

### 2. Generate Architecture Docs

Load [references/architecture-docs.md](references/architecture-docs.md) for templates and process.

### 3. Generate How-To Guides

Load [references/how-to-guides.md](references/how-to-guides.md) for templates and process.

Read roles from config.toml -> `config.docs.roles`. Generate one guide per affected role.

### 4. Parallel Generation

When invoked by target, architecture docs and how-to guides can be generated in parallel via separate agents since they have no dependencies on each other.

## Integration with Target

Target invokes this skill as the docs phase. It passes:
- Feature name and description
- Changed files list
- Affected user roles (from config.toml)

## Key Principles

- **Location matters** - Follow configured paths from config.toml
- **Audience-appropriate** - Developers get architecture, users get how-to
- **Keep current** - Update existing docs, don't duplicate
- **Roles from config** - Only generate for configured roles

## Completion contract

Docs files written to disk are the proof this phase happened. No artifact write and no `fno gate` call is needed. If no docs were written (no affected roles configured), report that clearly so the operator can provide explicit role configuration and re-run.

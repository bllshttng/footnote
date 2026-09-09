# Abilities How-To

Create **end-user** how-to guides. These are for your app's user roles - NOT developers.

For developer/architecture docs, use `/docs` instead.

## Reference Materials

Load references as needed when writing guides:

| Reference | Load When | Content |
|-----------|-----------|---------|
| [references/guide-template.md](references/guide-template.md) | Writing a new how-to guide (Step 3) | Full template with frontmatter, required sections, formatting rules |
| [references/content-elements.md](references/content-elements.md) | Adding screenshots, common issues, reader testing (Step 4) | Screenshot descriptions, common issues boxes, pro tips, reader testing prompts |

## Step 0: Scope the Guides

How-to guides run when the ship-docs work order requests them - there is no config gate (the retired doc keys are not in the schema; do not read or write them). The request supplies the audience roles and may override the directory.

### Locate How-To Directory

Default `docs/howto`; the request may override it.

```
{how_to_path}/{role}/{feature}.md
```

### Discover Roles from Config

Read the roles from the work order (the request names the app's user roles):

```yaml
roles: [admin, user]  # from the request; your app's user roles
```

If the request names no roles: write one guide set with no role split.

### Check for Existing Guides First

```bash
# ALWAYS check if a guide already exists for this feature
# Check each role from the work order's role list
ls {how_to_path}/{role}/
```

**If a guide exists:** UPDATE it (add new sections, revise outdated info). Do NOT create a new file.

**If no guide exists:** Create at `{how_to_path}/{role}/{feature}.md`

## Philosophy

**Write for real humans, not developers.** How-to guides should:
- Use plain language (no jargon)
- Show exactly what users will see
- Answer "how do I...?" questions directly
- Catch "curse of knowledge" blind spots via reader testing

## Target Audiences

Roles come from the work order. Example:

| Role | Directory | Concerns | Tone |
|------|-----------|----------|------|
| admin | `{how_to_path}/admin/` | Setup, configuration, managing others | Professional, comprehensive |
| user | `{how_to_path}/user/` | Daily tasks, core workflows | Friendly, action-oriented |

## Process

### 1. Gather Context

```bash
# Check existing guides for this feature across all roles
ls {how_to_path}/{role}/
```

Explore the codebase to understand:
- What can users actually do?
- What UI do they see?
- What are common workflows?

### 2. Identify Audience & Tasks

For each audience, list:
- **Entry point**: How do they first encounter the app?
- **Core tasks**: What do they do daily/weekly?
- **Rare tasks**: What do they need occasionally?
- **Error recovery**: What goes wrong?

### 3. Write Using Guide Template

Load [references/guide-template.md](references/guide-template.md) for the full template structure and required sections.

### 4. Reader Testing (Critical)

Load [references/content-elements.md](references/content-elements.md) for reader testing prompts and content patterns (screenshots, common issues boxes, pro tips).

### 5. Fix Blind Spots

For each issue found:
1. Add missing context
2. Clarify navigation paths
3. Explain UI states explicitly
4. Answer implicit questions

### 6. Save Guide

**Output path:** `{how_to_dir}/{role}/{feature}.md`

**One file per role per feature.** If a feature is relevant to multiple roles, create separate files with audience-appropriate language.

**NEVER save how-to guides in:** project `src/` or other non-requested locations. They always go in `{how_to_dir}/{role}/`.

## Quality Checklist

Before finalizing:
- [ ] Can someone complete the task with ONLY this guide?
- [ ] Are navigation paths explicit ("Dashboard > Staff > Add")?
- [ ] Are button/UI element names exact matches?
- [ ] Did fresh Claude reader testing find issues?
- [ ] Are common errors addressed?
- [ ] Is jargon explained or removed?

## Maintenance

Update guides when:
- UI changes (button names, navigation)
- New features launch
- User feedback indicates confusion
- Workflows change

## Key Principles

- **User's perspective first** - What are THEY trying to do?
- **Progressive disclosure** - Quick start, then details
- **Reader testing required** - Fresh eyes catch blind spots
- **Action-oriented** - Focus on "how to" not "what is"
- **Plain language** - No technical jargon

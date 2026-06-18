---
name: megatron
description: "Author cross-project fleet missions. Use when: 'ship across projects', 'coordinate web + backend', 'fleet mission', 'multi-repo orchestration', 'cross-project rollout'."
argument-hint: "<mission-goal>"
requires:
  binaries:
    - "fno >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.30"
---

# Megatron

Cross-project fleet orchestration authoring. Sits one altitude above
`/megawalk`: projects (not tasks) are the wave units, dispatched via
the inbox substrate.

This skill is the conversational front-end. It walks you through five
discovery questions, drafts a mission manifest, validates it, and
adopts the mission to the backlog as a fleet-mission node. Execution
happens via `fno megatron run <mission-id>`.

## When to use

Use `/megatron` when the work spans **two or more projects** that need
to coordinate explicitly: discoveries from project A inform project B's
work, and the timing matters. Examples:

- "Ship state CO end-to-end" - regulatory parser + frontend + ops all
  need to land together
- "Rotate the API key across the fleet" - shared secret update across
  every service
- "Migrate auth from session cookies to OIDC" - per-project work with
  shared schema decisions

If the work lives in one project, use `/blueprint` instead.

## Process

### Step 1: Parse the mission goal

The positional argument is the user's headline goal. Capture it as
``MISSION_GOAL`` and use it as the default for question 1 below.

### Step 2: Discovery questions

Ask these five questions in order, one at a time, with AskUserQuestion.
The reference at ``references/discovery-questions.md`` carries the
exact wording, defaults, and example answers. Questions:

1. **Mission goal** - default: the positional argument
2. **Participating projects** - multi-select from
   ``work.workspaces.<workspace>.projects`` keys in ``settings.yaml``
3. **Wave breakdown** - free-form description of sequencing; the LLM
   maps it to ``waves: [...]``
4. **Research wave needs** - per-wave yes/no; default no
5. **Failure policy / autonomy** - default ``failure_policy: block``,
   ``autonomy_level: cautious``

If the user types ``/cancel`` mid-discovery, exit with "Mission
authoring cancelled. No changes saved." and do not write any files.

### Step 3: Draft the manifest from the template

Render ``references/manifest-template.md`` with the gathered answers.
The template's frontmatter matches the schema at
``cli/src/fno/megatron/manifest.py``. The body is human-facing
prose explaining the mission to the operator.

Mission id is generated as ``ab-`` plus 8 hex characters (the same
shape as backlog ids). Slug is ``YYYY-MM-DD-<sanitized-goal-fragment>``.

### Step 4: Validate the draft

Run the validator before saving:

```python
from fno.megatron import load_manifest, validate_manifest

manifest = load_manifest(draft_path)
errors = validate_manifest(manifest)
```

If ``errors`` is non-empty, surface every entry to the user. Offer
three options: edit-in-conversation, save-as-draft (no intake), or
cancel. Do NOT save a malformed manifest to the canonical fleet
directory.

### Step 5: Save and adopt

When validation is clean:

```bash
TARGET_DIR="$HOME/.fno/fleet/$SLUG"
mkdir -p "$TARGET_DIR"
mv "$DRAFT_PATH" "$TARGET_DIR/00-INDEX.md"
fno backlog intake "$TARGET_DIR/00-INDEX.md"
```

The intake reads the manifest's frontmatter and creates a graph node
with ``type: fleet-mission`` and ``mission_id: ab-XXXXXXXX``. The
operator can then run:

```bash
fno megatron run ab-XXXXXXXX
```

### Step 6: Print handoff

Always end with a single line the user can copy:

```
Mission ab-XXXXXXXX adopted. Run `fno megatron run ab-XXXXXXXX` to execute.
```

## Failure modes (handle inline; never silently no-op)

- **Empty wave (``empty_wave``)**: re-prompt for that wave's content.
- **>8 projects (``wave_project_cap_exceeded``)**: ask user to split
  into two waves or remove a project. Cap value lives in
  ``config.megatron.max_projects_per_wave`` (default 8).
- **Research-after-research (``research_chain``)**: ask user to
  interleave a non-research wave or change one wave's ``wave_type``.
- **Body oversize (``body_oversize``)**: ask user to truncate the
  offending project's body to under 10KB at a section boundary.
- **Validate failure for any other reason**: print the full error list
  and offer edit / save-as-draft / cancel.

## See also

- ``cli/src/fno/megatron/cli.py`` - the CLI surface
- ``cli/src/fno/megatron/loop.py`` - the commander loop
- ``cli/src/fno/megatron/validator.py`` - validation rules
- ``skills/megawalk/SKILL.md`` - the single-project layer below

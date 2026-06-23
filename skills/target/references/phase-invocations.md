# Phase Invocations

**Load when:** dispatching a phase. Covers the phase routing table, invocation logic, scratchpad writes, confirmation check, Linear status sync, and the validate-phase artifact write.

## Phase Invocation Table

Phases resolve from `domain_phases` in settings.yaml (falling back to built-in code domain defaults):

| Phase | Default Skill (code domain) |
|-------|-------|
| think | `fno:think` |
| plan | `fno:plan` |
| do | Resolved from `domain_phases.execute` |
| review | Resolved from `domain_phases.review` |
| validate | Resolved from `domain_phases.validate` |
| docs | Resolved from `domain_phases.docs` (runs BEFORE ship so docs land in the same PR) |
| ship | Resolved from `domain_phases.ship` (pre-requisites: docs + browser testing done or skipped) |
| external | Resolved from `domain_phases.external` |

## Phase Invocation Logic

For each phase, read the resolved skill/command from `domain_phases` in target-state.md:
- If value is a skill name (contains `:`): invoke via the Skill tool
- If value is a bash command (contains spaces or starts with a command): run via Bash
- If value is `"none"`: skip phase (the corresponding skip flag in the manifest controls this)
- Think and spec phases are NOT domain-resolved (always use fno:think/blueprint)

## Phase Details Table

| Phase | Condition | Skill |
|-------|-----------|-------|
| 1. Think | `input_type == idea` | `fno:think` |
| 2. Plan | idea OR no 00-INDEX.md | `fno:plan` |
| 3. Do | `cross_project: false` (all new plans) | `domain_phases.execute` (default: `fno:do waves`) |
| 3. Do | `cross_project: true` (legacy only) | Migration shim — the cross-project pipeline was removed. WARN + route to spawn-into-project (see SKILL.md "CROSS-PROJECT IS RETIRED"); then run `domain_phases.execute` for this session's own project. Do NOT invoke a cross-project pipeline skill. |
| 3.5 Clean | Only with `clean` modifier | `/simplify` on changed files |
| 4. Review | Always (BEFORE PUSH) | `domain_phases.review` (default: `fno:review`) |
| 5. Validate | Always (BEFORE PUSH) | `domain_phases.validate` (default: project-detected); CI green on the PR is verified by the loop-check verb at promise time |
| 5.5 Docs | **Default: YES** (skip only with `--no-docs` or config) | `domain_phases.docs` (default: `fno:ship-docs`); docs MUST land BEFORE ship so they ride in the same PR |
| 6. Browser | If `has_ui` (skip with `--no-browser`) | `fno:tdd` (browser-testing reference); advisory run-and-log, never gates completion and is not a loop-check input; run BEFORE ship so any findings ride in the same PR |
| 7. Ship | Default YES (skip with `--no-ship`) | `domain_phases.ship` (default: `fno:pr create`); run AFTER docs + browser |
| 7a. Pre-ship rebase | Only if `auto_merge_approved: true` | `fno pr rebase` |
| 8. External | **Default: YES** (skip only with `--no-external` or config) | `domain_phases.external` (default: `fno:pr check {pr_number}`) |
| 8a. Post-review merge | Only if `auto_merge_approved: true` AND external review done | `fno pr merge --invoker=target "$PR_NUMBER"` |

See [auto-merge-mechanics.md](auto-merge-mechanics.md) for the full pre-ship rebase + post-review merge protocol.

## Pre-Invocation: Confirmation Check

Before invoking any resolved skill, check for `confirm: true` in its frontmatter:

1. If the resolved phase value is a skill name (contains `:`):
   a. Read the skill's SKILL.md frontmatter
   b. If `confirm: true` is present:
      - **Interactive mode:** Use AskUserQuestion with the skill's `confirm_message` (default: "About to run {skill-name}. Proceed?")
      - **Autonomous mode (claw):** Return BLOCKED with reason: "Skill {name} requires human confirmation (confirm: true)"
   c. If `confirm: true` is absent: invoke normally

2. If the resolved phase value is a bash command: no confirm check (commands are always safe because the user defined them in their config)

This check applies to the review, validate, ship, external, and docs phases — any phase that uses a domain-resolved skill. The execute phase uses `/do waves` which doesn't need confirmation.

## Linear Status Sync (optional - requires linear plugin)

If the linear plugin is installed and the plan has a `linear:` field in 00-INDEX.md, sync status at phase transitions:
- `/do waves` start sets "In Progress"
- After `/review` syncs progress
- After `/pr create` adds PR link comment
- After docs sets "Done"

If the linear plugin is not installed, skip all Linear sync steps.

## Validate Phase (external-truth gate)

`validate` is a bash command (from `domain_phases.validate`), not a skill invocation. Run it, and on a non-zero exit loop into the validation-failure-recovery flow ([failure-recovery.md](failure-recovery.md)) to either rollback to the pre-execute checkpoint or fix forward. The local run is the work - it is how failures get caught and fixed BEFORE pushing.

The `output_validated` GATE, however, reads external truth (control-plane collapse step 1, ab-10cb7d28): the stop hook checks `gh pr checks` on the recorded PR at promise time. There is no validate artifact, no provenance requirement, and no verifier to satisfy - CI green on the PR is the gate; CI red or pending blocks the promise regardless of any state boolean. Do not write `.fno/artifacts/validate-*.md`; nothing reads it.

After the validate command exits 0 the local run is done. The loop-check verb verifies CI on the PR at promise time; there is no state boolean to write.

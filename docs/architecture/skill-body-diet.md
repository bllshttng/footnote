# Skill Body Diet

A structural pattern for keeping `SKILL.md` files lean by extracting per-topic detail into `references/`. Applies whenever a skill body grows beyond the reliable-attention threshold for a single-file load.

## The problem

Every Claude Code skill invocation re-loads its `SKILL.md` into the prompt. The longer the file, the more attention the LLM spreads across it on every invocation. Past ~500 lines, two failures show up:

1. **First-screen rules dilute.** State machine rules and FORBIDDEN markers that fire in the first 30 seconds of execution sit alongside dense reference material that fires only in narrow phases. The LLM spends attention on both equally.
2. **Cost compounds.** A 1746-line skill body loaded by every `/target` invocation is the same input tokens, every time. Cached input is cheap, but that does not mean it should grow without limit.

`skills/target/SKILL.md` reached 1746 lines and `skills/megawalk/SKILL.md` reached 803 lines before the 2026-04-29 diet. Both were past the threshold.

## The pattern

Keep the `SKILL.md` lean. Move detail into `skills/{name}/references/{topic}.md` files that the skill loads on demand.

The split heuristic is one question: **"would a fresh-context invocation need this in the first read?"**

- **Yes** → keep in `SKILL.md`.
- **No** → move to `references/{topic}.md`.

What stays in `SKILL.md` after a diet:

- Title, one-line description, frontmatter
- State machine rules (allowed statuses, golden rule, completion gates)
- Three-factor gate verification headline + the canonical table
- Pipeline overview diagram (the table-of-contents)
- Philosophy + skill-composition table
- Usage section (basic invocation forms)
- Process steps as a one-line outline with pointers
- Atomic commit discipline (load-bearing every run for M/L)
- Cross-project hard gate (NON-NEGOTIABLE markers)
- Pre-promise gate-audit headline
- Post-promise behavior contract (STOP IMMEDIATELY)
- State files table
- References list

What moves to `references/`:

- Phase invocation tables and routing logic
- Per-phase bodies (clean, review, goal-verify, direction-alignment)
- Failure-recovery flows (validation-failure, circuit breaker)
- Pre-promise sequence detail (cost calculation, handoff, registry, stamp)
- Auto-merge mechanics (rebase + post-review merge)
- Settings YAML schema
- Resume protocol detail
- Linear integration, scratchpad writes, confirmation check, model fallback

## The reference-loading pattern

Each extracted section becomes its own reference file:

```
skills/target/SKILL.md
skills/target/references/
├── init-state.md
├── phase-transition-guards.md
├── phase-invocations.md
├── phase-bodies.md
├── failure-recovery.md
├── pre-promise.md
└── ...
```

Naming is lowercase + hyphenated + descriptive (`phase-transition-guards.md`, not `ptg.md`). The directory is flat - no nesting - for ease of grep and discovery.

In `SKILL.md`, the extracted section is replaced by a short pointer:

```markdown
The transition matrix (which gates must hold before each transition) lives in
[references/phase-transition-guards.md](references/phase-transition-guards.md).
The acceptance-criteria gate that runs before `/do waves` is documented there too.
```

The LLM treats this as "if the surrounding work needs the detail, read the reference; otherwise skip it." This is the standard pattern across the footnote skills - the diet extends the convention rather than inventing a new one.

## De-duplication across skills

Cross-skill content (cross-project coordination, discovery gate, kill criteria, completion gate protocol) lives canonically in its **owning skill's** `references/` dir and is bundled into each consumer's `references/` at build time via `skill-bundles.yaml`. Per-skill references stay in `skills/{name}/references/`. The split is by audience:

- Used by exactly one skill → `skills/{skill}/references/{topic}.md` (canonical, not bundled)
- Used by two or more skills → canonical in the primary owner's `skills/{owner}/references/{topic}.md`; each consumer declares a `references:` bundle entry sourcing it (e.g. `iteration-loop.md` is owned by `target` and bundled into `fix` and `think`; `worktree.md` is owned by `target` because "new target -> new worktree")

Before creating a new reference, check the owning skill's `references/` for an existing canonical version. Pointers from the lean `SKILL.md` should target the local (bundled) `references/{topic}.md`, never a cross-skill path escape.

## The behavior-parity bar

A diet must preserve behavior. The verification bar is not a code-level test suite - it is a smoke test that compares first-screen invocation output before and after.

Concrete check:

1. `wc -l skills/{name}/SKILL.md` shows the lean target (typically <500 lines).
2. `ls skills/{name}/references/` shows the expected count of new reference files.
3. `grep -rEn "{name}/SKILL\.md:[0-9]+"` across the repo returns no broken line-number citations (any survivors got updated to point at references). `-E` enables extended regex so `[0-9]+` correctly matches multi-digit line numbers; with default BRE, `[0-9]+` would match a literal `+`.
4. End-to-end smoke test: invoke the skill in a clean test directory and verify the first 100 lines of behavior match the pre-diet output (saved snapshot from before the change).
5. Repo-wide test suite passes. No test should depend on a specific `SKILL.md` line number; if any do, those tests are broken in their own right and need fixing.

## When NOT to diet

- A skill under ~500 lines is already in the comfort zone. Don't extract for the sake of extraction.
- Don't bundle a content rewrite with a diet. The diet is a *move*, not a *rewrite*. If the extracted content is unclear or outdated, that is a separate spec.
- Don't extract content the LLM needs in every invocation. State machine rules, FORBIDDEN markers, MANDATORY clauses, and other load-bearing-on-every-run content stays in `SKILL.md`. The diet protects them by reducing the surrounding noise, not by moving them away.

## When to diet next

Other long skills should follow the same pattern when they cross the threshold. Candidates that may need a diet eventually:

- `skills/blueprint/SKILL.md` (currently in the same range as pre-diet target)
- `skills/do/references/waves.md` (full wave-orchestration body)
- Anything else that crosses 500 lines of body without strong load-bearing-every-invocation justification

A future enhancement could be `scripts/validate-skill-size.sh`: a pre-commit hook that errors when any `SKILL.md` exceeds 500 lines without an explicit `# size-exempt: <reason>` directive in frontmatter. Out of scope for the initial diet.

## Audience split (optional follow-up)

When a skill has content that does not fit either bucket - too narrative for `references/`, not load-bearing-every-invocation enough for `SKILL.md` - the resolution is a third file:

```
skills/{name}/SKILL.md          # LLM-facing, lean
skills/{name}/references/*.md   # LLM-on-demand, per-topic
skills/{name}/docs/{NAME}.md    # human-facing, narrative
```

The `docs/` file is for human readers studying the skill's design - not loaded by the LLM at invocation time. Target's diet did not require this third tier; if it had narrative orphans after the extraction, the next step would be to create `skills/target/docs/TARGET.md`. The orphan check came up clean, so the audience split stayed at two tiers.

When SKILL.md serves LLM + humans + contributors, splitting by audience is the structural fix - renaming or rewriting to fit one audience does not work.

## Diet results (2026-04-29)

| Skill | Pre-diet | Post-diet | Reduction | New references |
|-------|----------|-----------|-----------|----------------|
| `skills/target/SKILL.md` | 1746 lines | 363 lines | 79% | 13 (phase-transition-guards, phase-invocations, phase-bodies, failure-recovery, pre-promise, init-state, scratchpad-writes, secondary-repo-commit, auto-merge-mechanics, model-fallback, resume, settings, usage-detail) |
| `skills/megawalk/SKILL.md` | 803 lines | 323 lines | 60% | 6 (argument-parsing, roadmap-generation, adopt-protocol, bare-loop-execution, context-injection, plan-freshness) |
| **Total** | **2549 lines** | **686 lines** | **73%** | **20 new references** |

Behavior parity verified by smoke test (silent-failure-hunter + code-reviewer agents both returned no findings >= 80 confidence).

## File map

| File | Role |
|------|------|
| `skills/target/SKILL.md` | Lean orchestrator (363 lines). Load-bearing-every-invocation rules only. |
| `skills/target/references/` | 14 new + 18 pre-existing per-topic detail files. |
| `skills/megawalk/SKILL.md` | Lean orchestrator (323 lines). HARD-GATE + state machine + design principles + subcommand outlines. |
| `skills/megawalk/references/` | 6 new + 6 pre-existing per-topic detail files. |
| `skills/target/references/auto-merge.md` | Auto-merge protocol (owned by target). |
| `docs/architecture/megawalk-migration.md` | Removed-form migration history (owned by megawalk). |
| `docs/architecture/skill-body-diet.md` | This document - the pattern itself. |

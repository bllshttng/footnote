# The full pipeline + phase philosophy

Read this for a from-idea or multi-phase run when you want the whole phase map and the compose-don't-hardcode rationale. A ready-node run that already has a plan does not need it - the spine covers the happy path.

## The Full Pipeline

```
"I want an AI chat feature"
         │
         ▼
┌─────────────────────────────────────────────────────────────┐
│  /think          → Design thinking, explore problem space   │
│  discovery gate  → Surface unknowns before planning         │
│  /blueprint      → Create implementation plan with waves    │
│  /do waves {expertise} → Execute with TDD (archer agents)    │
│  /simplify       → Remove AI slop patterns (clean modifier)  │
│  /review    → Internal quality gates                   │
│  validate        → Run tests / typecheck / build            │
│  /ship-docs      → Architecture docs + how-to guides        │
│  browser testing → If has_ui, run Chrome DevTools checks    │
│  /pr create      → Create PR (fork to Haiku)                │
│  /pr check       → Wait for external review + implement     │
│  auto-merge      → Optional, only if auto_merge_approved    │
└─────────────────────────────────────────────────────────────┘
         │
         ▼
   PR ready for merge (docs + browser verification included)
```

Docs and browser testing run BEFORE `/pr create` so they ride in the same PR, get reviewed alongside the code, and are included in any auto-merge. Historic versions of this skill ran docs last, which led to docs landing in a follow-up PR whenever `auto_merge_approved: true` tripped immediately after external review.

## Philosophy

**Compose, don't hardcode.** This skill orchestrates other skills:

| Phase | Skill Used | Purpose | When to Run | Model |
|-------|------------|---------|-------------|-------|
| Think | `/think` | Design exploration | If starting from idea | Opus (inline) |
| Plan | `/blueprint` | Create waves + tasks | If no plan exists | Opus (inline) |
| Execute |`/do waves` | Wave orchestration + TDD | Always | Opus (inline) |
| Clean | `/simplify` | Remove AI slop patterns | Only with `clean` modifier | Opus (inline) |
| Review | `/review` | Internal quality gates (BEFORE push) | Always | Opus (inline) |
| Validate | _(bash)_ | npm run build / pytest | Always | Opus (inline) |
| Docs | `/ship-docs` | Architecture + how-to in parallel | Default YES, skip with `--no-docs` or config - runs BEFORE ship so docs ride in the same PR | **Sonnet** (agents) |
| Browser | `/tdd` (browser-testing ref) | Human-like UI checks (advisory: runs and logs, never gates `<promise>`) | If `has_ui` - runs BEFORE ship | Sonnet (agent) |
| Ship | /pr create | PR creation (fresh agent) | Always | **Haiku** (agent) |
| External | `/pr check` | Wait for external review + implement | Default YES, skip with `--no-external` or config | Sonnet (review response), Opus (code fixes) |
| Auto-merge | `${SKILL_DIR}/scripts/lib/pr-merge.sh` | Merge after external approves | If `auto_merge_approved: true` | n/a (shell) |

See [usage-detail.md](usage-detail.md) for model-optimization rationale (when to keep Opus inline vs spawn cheaper agents).

**Phase applicability is judgment, not a gate.** Every phase above is available; run the ones the work needs. User skip flags (CLI) and project config (`.fno/config.toml`) still force-skip. Otherwise judge by what the change is:

- **/think + /blueprint**: only if you started from a bare idea, OR the bound plan is still design-stage (then `/blueprint` alone - the thinking is already done). A blueprint-complete plan skips straight to implement.
- **/do waves**: for a multi-task plan with parallelizable waves. A single-file or locked refactor runs **inline**, not through the wave orchestrator.
- **/simplify (clean)**: only with the `clean` modifier, or on AI-slop-prone new code.
- **/review**: run it; it is cheap insurance. For a tiny prose/config change a light self-review is enough.
- **/ship-docs**: skip for an internal refactor with no public API or architecture change; run it when behavior or a public surface changed.
- **browser testing**: only if `has_ui`.
- **/pr create + `<promise>`**: always. That is the deliverable.

When unsure whether ceremony applies, prefer running it. But never let "did every phase fire?" gate the promise - completion is the world (PR green + reviewed), not a phase checklist.

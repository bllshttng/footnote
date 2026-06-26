# AGENTS.md

This file provides project context and behavioral guidelines for AI agents (Claude Code, Gemini CLI, Codex CLI) working in this repository.

## Foundation: Karpathy Guidelines

Behavioral guidelines to reduce common LLM coding mistakes, derived from [Andrej Karpathy's observations](https://x.com/karpathy/status/2015883857489522876) on LLM coding pitfalls. These principles are the foundation of everything we do here.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" -> "Write tests for invalid inputs, then make them pass"
- "Fix the bug" -> "Write a test that reproduces it, then make it pass"
- "Refactor X" -> "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] -> verify: [check]
2. [Step] -> verify: [check]
3. [Step] -> verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## Repository Overview

This is the **footnote** Claude Code plugin - an autonomous delivery pipeline that takes features from idea to shipped PR. Think, plan, do, review, ship.

**First time here?** Configure the project with `fno setup wizard` (terminal, any CLI) or `/fno:setup` (in a Claude Code session). It is optional - defaults work, so `/fno:target "..."` runs without it - but it writes a validated `.fno/settings.yaml` (review bots, auto-merge, backlog prefix, vault, etc.). On Claude Code a first-run SessionStart hook nudges you toward this when no config exists yet; on Codex/Gemini this pointer is the nudge.

## Architecture

```
footnote/
├── .claude-plugin/          # Plugin manifest
├── skills/                  # Skills directory; see skills/using-abilities/SKILL.md for the advertised set
├── agents/                  # Subagents (target, code-reviewer, sigma-review specialists, etc.)
├── commands/                # Slash commands (cancel-target)
├── hooks/                   # Stop hooks, session-start, context monitor
├── scripts/                 # Validation, metrics, orchestration, codemap, diagnostics
├── tests/                   # Test harnesses
└── internal -> [symlink]    # Links to Obsidian vault for plans/docs
```

## Worktrees

Create worktrees at `~/conductor/workspaces/abilities/<name>` and nowhere else. After `git worktree add`, run `bash scripts/setup/setup-worktree.sh` to link the Obsidian vault symlink, `.fno/` state, and `.claude/` subdirs from canonical. `claude --worktree <name>` is intercepted by `.claude/settings.json`'s `WorktreeCreate` hook so it lands at the same canonical path automatically. Full contract (forbidden locations, link policy, cross-project exception): [.claude/rules/worktrees.md](.claude/rules/worktrees.md).

## Search hygiene

Prefer `rg` (ripgrep) or the harness Grep tool over Bash `grep -r`. `grep -r` ignores `.gitignore` and descends into nested checkouts under `.claude/worktrees/`, so a single `grep -r` from the repo root can return hundreds of false hits from an unrelated worktree's copy of the tree. `rg`, the Grep tool, and `fno codemap` all respect `.gitignore` (and `.claude/worktrees/` is gitignored), so they skip the nested checkout automatically. If you must use `grep -r`, scope it to a specific path rather than the repo root.

## Multi-CLI Support

Skills are portable across Claude Code, Gemini CLI, and Codex CLI. Orchestration features (target looping, subagent dispatch) require platform-specific hook configuration. This file (`AGENTS.md`) is the canonical source; `CLAUDE.md` and `GEMINI.md` are one-line stubs containing `@AGENTS.md` so each CLI inline-imports identical content via its native import syntax (Codex reads AGENTS.md directly).

For platform-substrate facts (hook events, frontmatter support, skill directories, `@file` import syntax per CLI) see [docs/HARNESSES.md](docs/HARNESSES.md). For how footnote wires into those hooks see [docs/architecture/multi-cli-hooks.md](docs/architecture/multi-cli-hooks.md). For per-skill compatibility consequences see [docs/SKILL-COMPAT-MATRIX.md](docs/SKILL-COMPAT-MATRIX.md). The per-CLI notes below cover only the headline behavior.

RTK ([rtk-cli](https://github.com/rtk-ai/rtk)) is a recommended companion for long autonomous loops; `/fno:setup` detects and wires it.

**Gemini CLI:** Defaults to sequential execution (no parallel subagent dispatch). Use `skills/target/references/cli-tool-mapping.md` for tool equivalents. If hooks are not configured, the manual path is `think` -> `plan` -> `do`. Experimental project agents in `.gemini/agents/` can upgrade execution when enabled.

**Codex CLI:** Uses project-scoped custom agents in `.codex/agents/` when available; degrades to sequential execution otherwise. Reference skills with the Codex pattern (e.g. `$fno:do`). Manual path if hooks are absent: `$fno:think` -> `$fno:blueprint` -> `$fno:do`.

## Provider Rotation

The `fno providers` command surface manages provider records (CLI + auth + credentials), failover, per-model lockouts, per-agent routing, named combos, and the Codex CLI runtime adapter. Configuration lives under `config.providers` in `~/.fno/settings.yaml` (global) or `.fno/settings.yaml` (project-local override).

See [docs/provider-rotation.md](docs/provider-rotation.md) for the full schema, surface tables (list/add/show/test/use/remove), failure-mode taxonomy, combo resolution priority, and per-spec migration notes. Not yet wired: `gemini.py` / `glm.py` / `openclaw.py` / `hermes.py` adapters (separate plans per CLI), per-segment cost-attribution math (Spec 2.5), lockout reason persistence, tuned hysteresis with health-check probes.

**Cross-model review.** The internal `/review sigma` panel can route individual agents to a different provider (codex/gemini) than wrote the code, as a model-diverse second opinion. Opt-in via `config.review.cross_model.enabled: true` OR an explicit `config.review.agent_providers` map (agent -> `claude|codex|gemini|alternate`); default is all-claude and byte-for-byte unchanged. `alternate` resolves to a provider differing from the implementer (read from the ledger), reusing the `config.providers` records + lockout state above; it degrades to claude when none differs. See [docs/architecture/cross-model-review.md](docs/architecture/cross-model-review.md).

**Role-based model routing.** A spawn's *role* selects its model at spawn time: auxiliary coordination roles (`coordinate|tidy|orient|consolidate`) route the worker to a secondary provider (z.ai GLM by default; DeepSeek or others by config) via its Anthropic-compatible endpoint (`ANTHROPIC_BASE_URL`/`AUTH_TOKEN` + all model-tier vars, so the whole worker bills the secondary pool), while production roles (`implement|review-verdict`) and the default (no role) stay on the primary Anthropic model, byte-for-byte unchanged. Pass `--role` to `fno agents spawn`. A hard guard refuses to route a protected role even via config; with no provider key configured a routed role fails safe to the primary model with a one-line notice. Config under `config.model_routing` (`providers` + `roles` + `extra_env`; keys live in env vars / `.env` files per provider, never in settings.yaml). See [docs/architecture/role-based-model-routing.md](docs/architecture/role-based-model-routing.md).

## Key Commands

**Front door.** Six verbs are the advertised front door: `/target`, `/megawalk`, `/think`, `/review`, `/pr`, `/fix`. Each fans out to modes: `/review sigma|peer`, `/fix` (default) + `investigate`, `/think` (default) + `what-if|panel`, `/pr create|check|merged`, and `/do flat|waves`. Everything else is invocable by its full name but not surfaced at the top. The advertised set lives in `skills/using-abilities/SKILL.md`, injected at SessionStart.

### Primary Workflow

| Command | Purpose |
|---------|---------|
| `/target "feature"` | End-to-end: think → blueprint → do → review → ship |
| `/target path/to/plan` | Execute existing plan (skips think/blueprint) |
| `/target <node-id>` | Execute a specific graph-backlog node by ID (e.g. `/target fno-a3f9`; resolves via `~/.fno/graph.json`). Same form works in `/megawalk`. |
| `/target L "feature"` | Large size: full ceremony including adversarial |
| `/target auto-merge "feature"` | Auto-merge PR once external review passes (opt-in; also works on `/megawalk`). Requires `config.auto_merge.enabled: true` OR the CLI modifier. See [skills/_shared/auto-merge.md](skills/_shared/auto-merge.md). |
| `/megawalk` (bare) | Loop through the ready backlog until done. Replaces the removed `continue` and `next` subcommands. |
| `/megawalk roadmap <vision.md>` | Generate a roadmap backlog from a vision doc, then loop. Replaces the removed top-level `vision.md` positional. |
| `/megatron "mission"` | Author a cross-project fleet mission via 5-question discovery wizard. Drafts a manifest at `~/.fno/fleet/{slug}/00-INDEX.md` and adopts to the backlog. See [docs/architecture/megatron.md](docs/architecture/megatron.md). |
| `fno megatron run <mission-id>` | Execute a fleet mission via the unified Rust loop (`fno-agents loop run --driver megatron`): each project is walked as a megawalk one altitude down, mission-scoped via `--mission`. Subcommands: `run`, `next`, `complete` (loop plumbing), `status`, `cancel`, `retro`, `list`, `reconcile`. `reconcile` detects filesystem-vs-PR completion drift (`--backfill` writes missing JSONs for confirmed-merged PRs; never clobbers; never auto-resumes). |
| `/fno:blueprint <doc-path>` | Mutate design doc in place: append Execution Strategy, File Ownership Map, kill_criteria (single-doc format, new default) |
| `/fno:blueprint quick "feature"` | Create quick single-file plan for bugs/small fixes |
| `/fno:do` | Execute a plan: `flat` (default, lightweight single-session) or `waves` (wave orchestration) |
| `/fno:fix investigate` | Scientific method bug hunting with hypothesis loop + BDD acceptance criteria |

> **Megawalk surface change (2026-04-20):** `continue`, `next`, and
> `adopt --batch` are removed; bare `/megawalk` is now the canonical way
> to enter the loop (modifiers still work: `/megawalk parallel`,
> `/megawalk auto-merge`, etc.); the top-level `vision.md` positional
> moved under `/megawalk roadmap <path>`.
> See [skills/_shared/megawalk-migration.md](skills/_shared/megawalk-migration.md).

**Lean blueprint (2026-05-18):** `/blueprint` now mutates the upstream design doc in place rather than creating a separate folder plan. Running `/blueprint <doc-path>` appends execution sections (Execution Strategy, File Ownership Map, kill_criteria) to the design doc and updates its frontmatter status to `ready`. The same file evolves through the full pipeline: `/think` → `/blueprint` → `/do` → `/review` → `/ship`. Workers receive scoped briefs via `fno plan brief <plan-path> --task <id>` rather than reading the full doc directly. Folder plans created before this change continue to work unchanged; the new single-doc format is the default for new plans. See [docs/architecture/lean-blueprint.md](docs/architecture/lean-blueprint.md).

**Plan Mode front door (2026-06-02):** after you approve a plan in Claude Code's native Plan Mode (Shift+Tab → `ExitPlanMode`), a PostToolUse hook captures it to `.fno/.pending-plan.md` and the next bare `/target` detects it, backfills the structure target's gates require (`## Failure Modes` + the 5 BDD ACs, then `/blueprint`), shows you what was added, and on your `[y/N]` confirm executes it. The native plan supplies intent; footnote supplies rigor (gates are never relaxed). Claude-Code-only: on other CLIs the matcher never fires and `/target` is unchanged; headless/megawalk runs skip the attended-only front door. See [docs/architecture/target-plan-mode-integration.md](docs/architecture/target-plan-mode-integration.md).

### Supporting Commands

| Command | Purpose | Context |
|---------|---------|---------|
| `/fno:think` | Design exploration before planning | main |
| `/fno:think panel` | Multi-persona product debate (5-8 experts) | main |
| `/fno:think what-if` | Scenario exploration, edge cases, and failure modes before building | main |
| `/fno:review` | Run review agents on changes | main |
| `/fno:tdd` | Test-driven development | main |
| `/fno:fix` | Autonomous fix loop with one fix per iteration and auto-revert on regression | main |
| `/triage` | LLM-proposed ordering for pending specs (dependencies, priorities, duplicates) | main |
| `/pr create` | Create a PR from commits (dispatches the Haiku `pr-creator` subagent) | **main** |
| `/pr check` | Poll for external review, implement feedback | main |
| `/setup` | Interactive settings.yaml wizard | main |

## Execution Model

### Wave-Based Orchestration

Plans use waves defined in `00-INDEX.md`:

```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    tasks: [1.1]
  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2, 2.3]
```

The orchestrator (`skills/do/orchestrator.py`) routes tasks to specialized agents based on keywords:

| Keywords | Agent |
|----------|-------|
| frontend, react, ui, component, tailwind | target (frontend) |
| backend, api, supabase, auth, database | target (backend) |
| devops, docker, ci/cd, deploy, terraform | target (devops) |
| etl, pipeline, data, analytics | target (data) |

### Per-task executors (operator-managed)

Operator resolves each task's executor via a three-tier chain: explicit `executor:` on the task block, then on plan frontmatter, then surface inference (files matching `**/*.{tsx,jsx}`, `[**/]components/**`, `[**/]routes/**`, `[**/]src/styles/**` resolve to `impeccable`; `app/` is intentionally NOT a directory match). Fallback is `do` (default).

Recognized: `do` / `tdd` (archer, TDD-disciplined, default), `impeccable` (frontend-executor, full /impeccable pipeline with shape brief, stage selection, two-tier exit verdict 35/40 target, 25/40 floor). Pin-only treatments (animate, delight, colorize) require an explicit `impeccable_stages:` pin.

Audit findings (a11y, perf, responsive, visual consistency) gate **independently** from sigma-review's `quality_check_passed`. Both run; both are independent. See [skills/do/references/executor-resolution.md](skills/do/references/executor-resolution.md) for the full chain, locked surface inference list, override paths, and the audit-vs-sigma-review boundary.

### Backlog Vocabulary

Since 2026-04-22 the v2 CLI exposes the feature graph under the
canonical `fno backlog` namespace. The legacy `fno graph` spelling
remains as a deprecated alias (hidden from top-level help) so old
call sites keep working; new code should use `backlog`. Both names
resolve to the same Typer app so behavior is byte-identical.

Node IDs are minted as `<prefix>-<hex>` (e.g. `fno-a3f9`). The prefix and hex
width are set at `fno setup` and stored in `config.backlog.id_prefix` /
`config.backlog.id_hex_width` (lowercase prefix ≤7 chars, not `cv-`/`fu-`/`tgt-`;
width 4-8, default 4 at setup). Generation is strict (config-driven); resolution
is format-agnostic (a graph lookup), so any node ID resolves regardless of the
prefix/width it was minted under. An unconfigured install mints `ab-` + 8 hex.

Lifecycle phrase: `intake → triage → ready/next → done`.
Side states: `blocked` (open dependency) and `deferred` (explicitly paused
via `defer`; reversible via `undefer`).

| Canonical | Deprecated alias | Purpose |
|-----------|------------------|---------|
| `fno backlog` | `fno graph` | sub-app namespace |
| `fno backlog intake <plan>` | - | pull an existing plan file in as a node |
| `fno backlog triage <verb>` | - | reasoning adviser loop (context/propose/validate/apply/projects) |
| `fno backlog next` / `ready` | - | pick the highest-priority unblocked node (add `--include-deferred` to surface paused nodes) |
| `fno backlog done <id>` | - | mark a node complete (sets `completed_at`, unblocks dependents) |
| `fno backlog defer <id> --reason "..."` | - | pause a node with a rationale (sets `deferred_at` + `deferred_reason`; derives `_status: deferred`) |
| `fno backlog undefer <id>` | - | reverse a defer (clears the fields; node returns to `ready` or `idea` per cascade) |
| `fno backlog collisions check <plan>` | - | check a plan file or folder for file overlap with pending nodes (`--json` for structured output) |
| `fno backlog decompose <epic> --groups <json>` | - | bounded epic decomposition: atomic + idempotent upsert of group child nodes (`parent=epic`, `plan_path=<doc>#group-<slug>`, inter-group `blocked_by`). A group may carry an optional `project` (cwd resolved from the settings work-map; unmapped is refused) or explicit `cwd` to route that child into a different repo for a multi-repo feature; absent, it inherits the epic's repo. `--max-prs N` caps the count (falls back to `config.blueprint.max_prs_per_epic`, default 4); `--force` allows orphaning an already-shipped group. Driven by `/blueprint group N`. |
| `fno backlog triage health` | - | aggregate health metrics: idea pile, stale ready, failure-prone, all-pairs collisions, acknowledged-resolved nudges (`--json` for structured output) |
| `fno backlog maintain [--apply]` | - | recurring hygiene sweep: deterministic re-scope + leak-prune + auto-defer failure-prone nodes (#34) apply under `--apply`; dedup / drain-stale / cap-Now always propose-only. Skips live-claimed nodes; appends a summary to health-history. See "Backlog Maintenance Ritual". |
| `fno backlog supersede <new> --replaces <old> --reason "..."` | - | mark old node as superseded by new (sets `superseded_by` + `supersedes`; auto-defers old; derives `_status: superseded`) |
| `fno backlog rank <id> --top\|--bottom\|--before <id>\|--after <id>\|--clear` | - | curate a node's position within its `(column, project)` board lane. Sets a nullable-float `rank` ordered ahead of the `(priority, created_at)` fallback. `--before`/`--after` need a *ranked* anchor in the same lane (seed with `--top`); cross-lane / unranked anchors are rejected. `--clear` rejoins the unranked flow. Rank never changes a node's column. |
| `fno backlog advance [--closed <id>] [--project P]` | - | merge-triggered auto-continue: dispatch a fresh background `/target no-merge` worker for the next now-unblocked node after a PR merge. Opt-in (`config.auto_continue.enabled`, default off; `/megawalk auto-continue` arms a campaign) and decoupled from the loop driver, so megawalk / `/target` / `/megatron` inherit it. Called by `reconcile` + `/pr merged`. Non-fatal; emits exactly one decision event (`advance_dispatched` / `advance_skipped{reason}` / `advance_failed`). See "Merge-Triggered Auto-Continue". |
| `fno backlog get <id\|slug\|bare-hex>` | - | resolve a node by its canonical `<prefix>-<hex>` id, its title-derived **slug**, or a bare hex (re-prefixed to the active prefix). The deterministic resolution tiers used by `/agents spawn`'s VALIDATE step. |
| `fno backlog backfill-slugs` | - | one-time, idempotent, lock-safe pass that assigns a title-derived slug to every node lacking one. Re-running is a no-op. New nodes are slugged automatically by every mutation; this is the explicit operator trigger for the legacy graph. |

### Node Slugs

Every graph node carries an additive, title-derived **`slug`** - a stable human handle (`ab-1a2b3c4d` -> `dashless-spawn`) that **leads in display** while `ab-{8hex}` stays the sole canonical key (graph keys, claims, `events.unit_id`, branch names are untouched - no migration). A slug is derived once when a node is first persisted (`store.ensure_slugs`, inside `locked_mutate_graph`), is **globally unique** (a deterministic collision suffix is baked in at assignment), and is **immutable** thereafter (a later title reword does not change it). `fno backlog ready` / `next` / `find` / `get` all lead with `slug (ab-id)`; a pre-backfill node shows the hex alone. The slug is also an accepted **resolution input** alongside two more id-free spawn-entry modes - `next` (top ready node) and a model-judged *describe-it* fuzzy match over title+slug+details (always confirms before launch). See `/agent` SKILL.md (NORMALIZE -> RESOLVE) and `skills/agent/scripts/normalize.sh` for the deterministic tiers.

The `done` verb coordinates with the plan-completion-stamp spec when it ships: if a node has `plan_path` set, `done`
best-effort invokes `scripts/lib/stamp-plan.py graduate` to stamp
frontmatter, non-fatal if the stamp script is absent.

### Backlog Priority

Priority tiers (`pN` where lower N = more urgent):

- **p0** - drop everything (production incidents, blocking bugs, hotfixes)
- **p1** - next-up (typically small in scope; can ship in <1 day)
- **p2** - normal (default; medium-scope work; current sprint or quarter)
- **p3** - long-tail (low priority; might never get to it; experimental)

Advisory size mapping (not enforced): p0 typically small, p3 typically
large. Use `--size S|M|L` to declare scope explicitly when it diverges
from the default tier expectation. Priority and size remain orthogonal
in the schema, so a critical migration can be `--priority p0 --size L`.

Migration note: rows created before 2026-04-28 may carry the legacy
`high|medium|low` vocabulary. The first mutation after that date
backfills them via `recompute_statuses()` (`high → p1`, `medium → p2`,
`low → p3`). New tier `p0` is reserved for "drop everything" and is
never assigned by backfill.

### Backlog Board Ordering

Both boards (`graph.md` Obsidian Kanban + `fno backlog view` HTML, auto-rendered on every mutation) order each column by one shared lane key: `(project_lane, rank_band, priority, created_at)`. Cards cluster **per project within a column** (swimlanes), and an optional curated **`rank`** (nullable float, set via `fno backlog rank`) floats a node to the front of its `(column, project)` lane ahead of the `(priority, created_at)` fallback. Rank is scoped per lane and never changes a node's column - `_kanban_column` stays the sole column authority. The md board labels each card `· <project>` (the Obsidian plugin is column-only, so per-card labels + clustered order are the ceiling); the HTML master board draws per-project sub-lane dividers and shows a soft WIP count per column.

Soft WIP caps are HTML-board-only and configured under `config.kanban.wip_caps` (a `column → int` map) in `~/.fno/settings.yaml` - read directly from the global file in the renderer (defensive: a malformed/negative/string cap degrades to uncapped, never raises, because the render fires inside `locked_mutate_graph`). When the `wip_caps` block is absent, defaults are `{now: 20, next: 50}`; other columns uncapped. A column over its cap renders its count with an overflow style. The md headings stay bare (`## Now`, no count) so the Obsidian Kanban plugin keeps per-column collapse state across re-renders.

**Board order == work order.** The lane key and *selection* share one rank definition. What the walker / active-backlog daemon works next comes from `fno backlog next` -> `make_selection_sort_key`, which prepends the **same** `_rank_band` term the board uses (`rank band -> epics-first -> priority -> created_at`). So `fno backlog rank <id> --top` floats a card on the board AND makes it run next, and an explicit rank overrides the epics-first heuristic. With no rank set, selection is byte-for-byte the prior epics-first -> priority -> created_at order, so `fno backlog reprioritize <id> p0` is still the lever for unranked work. The shared helper lives in `cli/src/fno/graph/_constants.py` so board and selection can never drift. See the "Board order == work order" section of [docs/architecture/backlog-board-ordering.md](docs/architecture/backlog-board-ordering.md).

### Backlog Health Monitoring

`fno backlog triage health --check` evaluates the deterministic health report against configured thresholds, exits 0 healthy / 4 breach, dispatches notifications, and appends to `~/.fno/health-history.jsonl`. Pair with `--quiet` for `/loop`-style use. `fno backlog triage trend [--days N]` prints first-vs-latest deltas per metric (deterministic, no LLM).

Configurable under `config.health_monitor.*` in settings.yaml: thresholds (idea pile, stale ready, failure-prone attempts, collisions, project/cwd mismatch - default 0, any pending node whose mapped project disagrees with its recorded cwd is a producer regression), notification surfaces (terminal / discord / webhook / log_only, with severity-based throttling), and history retention.

```bash
/loop 1h fno backlog triage health --check --quiet
```

### Backlog Reconcile Auto-Trigger

`fno backlog reconcile` closes backlog nodes whose PR merged outside the ship gate (manual GitHub merge, bare `gh pr merge`) and drops a retro sentinel so a later session captures follow-ups. It runs automatically on SessionStart - `hooks/reconcile-session-start.sh` fires a backgrounded reconcile (detached via `nohup`, so it never blocks) and surfaces the *prior* sweep's result as a reminder when it closed a drifted node. (The between-loop-iteration trigger died with `megawalk-stop-hook.sh` in step-5 group 2; during a walk, the `fno backlog done` gh cross-check now keeps node state honest at every close.)

The trigger uses a throttle stamp (`.fno/.reconcile-stamp`, ~15 min, override via `RECONCILE_THROTTLE_SECONDS`) via `scripts/lib/reconcile-throttle.sh`, so a burst of parallel sessions does not hammer `gh`. Reconcile always runs in mutate mode here (never `--dry-run`); writing the retro sentinel is the point.

For long-running terminals that are not megawalk loops, run the same sweep on an explicit cadence (mirrors the health-check loop above):

```bash
/loop 30m fno backlog reconcile
```

### Merge-Triggered Auto-Continue

When a node's PR merges, `fno backlog advance` dispatches a fresh background `/target no-merge` worker for the next now-unblocked node, so a merge-gated epic walks itself group-by-group across merges with no manual re-invocation. The trigger is the **merge event**, not the loop terminal: the walker correctly dies on `NoWork` when a no-merge PR ships (dependents stay `blocked_by` the unmerged PR), and the next group is dispatched later by `fno backlog reconcile` (the dominant, web-merge-aware path, fired detached on SessionStart) or `/fno:pr merged`, each calling `advance` after the node-close commits. Because the merge event drives it, megawalk / `/target` / `/megatron` all inherit auto-continue with no driver-specific code.

Opt-in, default off: `config.auto_continue.enabled` (local > global, mirroring `config.auto_merge`; a malformed block fails safe to disabled), or arm a campaign with `/megawalk auto-continue` (writes the per-project marker `.fno/.auto-continue-armed`). `advance` is non-fatal (never wedges the reconcile/post-merge it rode in on), emits exactly one decision event per run (`advance_dispatched` / `advance_skipped{reason}` / `advance_failed`), honors a live `walker:<root>` (no double-dispatch during a walk), and dedups via a `dispatch:<id>` TTL bridge token so one merge observed by multiple triggers dispatches the successor at most once. It never merges anything (auto-merge stays a separate opt-in). Phase 1 = the verb + reconcile/post-merge wiring; a launchd web-merge watcher (zero-latency) is the deferred Phase 2. Full design: [docs/architecture/merge-triggered-auto-continue.md](docs/architecture/merge-triggered-auto-continue.md).

### Backlog Maintenance Ritual

`fno backlog maintain` is a recurring sweep that keeps `graph.json` + the kanban board clean by composing existing verbs. Seven legs. Three are **deterministic** and apply under `--apply`: re-scope project/cwd drift (project-null, wrong-project, or worktree-path cwd, corrected against the settings workspace map - only project/cwd are ever changed, never priority/status), prune pytest-temp leak nodes (`cwd` under a temp dir), and **auto-defer failure-prone nodes** (#34) whose consecutive-failure streak is `>= config.backlog.maintain.max_failed_attempts` (default 3). Three are **judgment** calls and are ALWAYS proposal-only regardless of `--apply`: surface near-duplicate idea titles for human merge/supersede, propose a reversible `defer` for ideas older than `config.backlog.maintain.staleness_days` (default 30), and report a Now column over `config.kanban.wip_caps.now` (default 20) with a `triage propose` suggestion. The apply legs skip any node a live target session holds (a `node:<id>` claim) and batch under one `locked_mutate_graph` so the board renders once. Every run appends a summary to `~/.fno/health-history.jsonl`, so `fno backlog triage trend` shows the board trending cleaner. Best-effort: a malformed row is skipped, a single failed apply does not abort the rest, and an empty graph is a clean no-op. Read-only without `--apply` (reports what it would do); `--json` emits the structured candidate/applied sets. Run on a cadence:

```bash
/loop 1d fno backlog maintain --apply
```

The same detection backs the `cli/scripts/list_misscoped_graph_nodes.py` diagnostic (`--apply` there emits `fno backlog update` command lines rather than mutating).

**Failed-node cascade (#34).** The auto-defer leg bounds a node that repeatedly fails to ship: the streak is *derived* from the walker's existing `node_failed` / `node_closed{close=parked}` events (keyed on `data.unit_id`), reset by any success close (`node_closed{close=closed}`) or a `node_undeferred` boundary, so the policy lives entirely in the Python sweep (the Rust walker is untouched, off the loc-ratchet path). At threshold the node is deferred with the sentinel reason `auto-failure: <N> consecutive failed attempts` - it leaves `fno backlog next`, is fully reversible via `fno backlog undefer` (which resets the streak so a human-fixed node gets a fresh N attempts), and never re-surfaces to burn an iteration on every walk. A per-run blast-radius cap keeps a provider-outage mass-failure from deferring half the board (the truncation is logged, never silent). Stranded dependents are **surfaced, never mutated**: `fno backlog triage health` (and `--json`) carries an always-on "stranded by failed blocker" section listing the dependents of each `auto-failure`-deferred node (an absent section means "none stranded", not "not checked"); they recover automatically via normal `blocked_by` resolution when the blocker ships.

### Control-plane LOC ratchet

Every PR is measured for executable-LOC delta across the control-plane paths: `hooks/`, `scripts/lib/`, `skills/target/scripts/verifiers/`, `cli/src/fno/loop.py`, `cli/src/fno/gates/`, both `gate_reality_map.yaml` copies, and `crates/fno-agents/src/loop*` (forward glob). A positive delta fails CI unless the PR body contains a `loc-exception: <reason>` line AND the PR appends exactly one matching entry (with the computed delta and a non-empty reason) to `scripts/ci/loc-ratchet-trajectory.yaml`. Both factors are required; either alone fails. The gate is permanent (no sunset), fail-closed (any parse or environment error is a red check), and self-remediating (the failure output states the computed delta and the exact steps to declare an exception). Enforceability requires `loc-ratchet` to be a required status check in branch protection - operator action needed after ship. Full doc: [docs/architecture/loc-ratchet.md](docs/architecture/loc-ratchet.md).

### Post-merge ritual (`/fno:pr merged`)

`reconcile` + `retro run` close the node and file graph nodes, but never write the per-project prose follow-ups to that project's configured vault parking-lot path (the LLM-judgment step). `/fno:pr merged [pr]` collapses the whole self-merge ritual into one verb: resolve `config.post_merge.parking_lot_path` per repo (fail loud if unset, never guessed - vault-area name != project name), run `reconcile` + `retro run`, read the merged diff, append a dated prose section to `parking-lot.md` keyed by a `<!-- post-merge:pr-N -->` marker for idempotency, and file triage-worthy work via `fno backlog idea`. Reuses existing verbs (no new mutation primitives); the only new code is the read-only `config.post_merge` schema block + the skill. Phase 2 (a per-repo launchd watcher that fires the skill headlessly on web-button merges) is deferred - it installs a plist and is gated on operator review. See [docs/architecture/auto-post-merge-ritual.md](docs/architecture/auto-post-merge-ritual.md). Not the same as the `fno mail` cross-project bus.

### Target self-handoff

Sanctioned session succession at pipeline boundaries via `skills/target/scripts/handoff.sh`: a `/target` session that has completed blueprint can hand the do phase to a fresh-context successor rather than carrying blueprint baggage through execution, and a do-phase worker crossing a wave boundary at high context usage can relay to a next-generation worker in the same worktree. The trigger policy is structural at blueprint->do (auto unattended, one-line `[Y/n]` confirm attended) and pressure-triggered at wave boundaries when `context_probe used_pct >= config.target.handoff.used_pct_trigger` (default 50); the generation cap (default 4) refuses further delegation and emits a `handoff-chain-exhausted` help instead. The delegated close works via manifest archival: `handoff.sh` archives `target-state.md` before the parent closes, and the Rust loop-check's missing-manifest path allows the session to exit (pinned by `crates/fno-agents/tests/loopcheck_missing_manifest.rs`); the `session_satisfied(trigger=delegated)` event is the audit record, not the unlock. See [docs/architecture/target-self-handoff.md](docs/architecture/target-self-handoff.md).

### State Files

Paths marked with a resolver use `fno.paths` for lookup; the default column shows the value when `~/.fno/settings.yaml` is absent or uses built-in defaults. Override any path in `config.paths.*` - see [docs/path-config.md](docs/path-config.md).

| File | Resolver | Default | Purpose | Owner |
|------|----------|---------|---------|-------|
| `paths.graph_json()` | `config.paths.graph_json` | `~/.fno/graph.json` | Feature dependency graph (backlog) | megawalk |
| `paths.graph_json()` (`.md` sibling) | derived | `~/.fno/graph.md` | Obsidian Kanban view (auto-rendered on every mutation) | roadmap-tasks.py |
| `paths.ledger_json()` | `config.paths.ledger_json` | `~/.fno/ledger.json` | Execution history (what happened, what it cost) | target |
| `paths.briefs_dir() / "{id}.md"` | `config.paths.briefs_dir` | `~/.fno/briefs/{id}.md` | Sidecar discovery briefs | megawalk |
| `.fno/target-state.md` | project-relative | `.fno/target-state.md` | Immutable session manifest (inputs-only; written once at init; `plan_path` first-fill via `fno state set` is the only legal post-init mutation) | target |
| `.fno/STATE.md` | project-relative | `.fno/STATE.md` | Wave/task progress | /do |
| `.fno/SUMMARY.md` | project-relative | `.fno/SUMMARY.md` | Task completion notes | operator |
| `.fno/00-INDEX.md` | project-relative | `.fno/00-INDEX.md` | Execution strategy | /blueprint |
| `{plan_path}.artifacts/` | plan-relative | `{plan_path}.artifacts/` | Sidecar folder for quick-plan artifacts (COMPLETION.md, `scratchpad-archive/`). Applies only to single-file plans; folder plans keep artifacts inside the folder. Session-state files (HANDOFF/SUMMARY/STATE) are transient and not archived. | target stop hook |

### Path Configuration and Migration

All user-data paths (`graph.json`, `ledger.json`, `briefs/`, `fleet/`, etc.) are resolved via `fno.paths`. The default is `~/.fno/` for global state and the project's `.fno/` for per-project files.

To check your current configuration, run:

```bash
fno config doctor
```

To regenerate the settings file from defaults (safe to re-run; idempotent):

```bash
fno setup migrate-paths --force
```

Full schema reference, env vars, and template variables (`{vault}`, `{project}`): [docs/path-config.md](docs/path-config.md).

### Ship vocabulary

"Ship" is overloaded; this is the one canonical disambiguation. Five distinct meanings:

| Term | What it is |
|------|-----------|
| `/ship` (the verb) | The deliverable-strategy umbrella: drive any deliverable to its finish line, dispatching on deliverable type. `/ship pr` = the PR lifecycle (= `/pr`, the retained permanent alias); `/ship doc` ships a research brief to `config.research.output_dir` + grades it. A thing is a ship type only if it has a mechanical *green*; ongoing areas (budget, community) are not - route them through `/target` / `/megawalk`. See [skills/ship/SKILL.md](skills/ship/SKILL.md). |
| ship phase | The `/target` pipeline phase that creates the PR (rebase + `/pr create`); runs after validate + docs + browser. |
| ship gate | The completion point where `/target` stamps the plan frontmatter (status, shipped_at, urls). "First ship completed (PR created)." |
| `DonePRGreen` / `DoneAdvisory` | Loop `TerminationReason`s: a code deliverable finishes at `DonePRGreen` (PR + CI + reviewed); a doc/advisory deliverable at `DoneAdvisory` (written + eval-green, no PR). These are the two finish lines `/ship` dispatches between. |
| `/ship-docs` | A documentation-generation skill (architecture docs + how-to guides), invoked as `/target`'s docs phase. It is NOT a `/ship` deliverable type - the name predates the umbrella; do not confuse the two. |

`fno pr merge` is the PR-merge CLI primitive, unrelated to the `/ship` verb (merge is one action inside the `pr` lifecycle, not a ship type of its own).

### Plan Completion Stamp

When `/target` completes the ship gate, it stamps the plan's frontmatter (`status: shipped|done`, `shipped_at`, `urls`, `session_ids`) so shipped plans are self-describing. Inline-list syntax only: the stdlib writer doesn't parse block-list indented frontmatter, so don't hand-edit `urls` / `session_ids` into block-list form.

- `shipped` = first ship completed (PR created). Single-project plans immediately become `done`.
- `done` = all expected ships complete. Cross-project plans reach `done` when `len(urls) >= len(projects)`.

Folder plans get a `COMPLETION.md` at ship-gate time and a `scratchpad-archive/` preserving the final target session. See [docs/architecture/plan-completion-stamp.md](docs/architecture/plan-completion-stamp.md).

### Multi-Repo Features (spawn-into-project)

The heavyweight `scope: cross-project` parallel-worktree pipeline has been
removed. A session works only in its OWN project; when work belongs to
another repo it is spawned into that repo's project, never edited from the
current session. A multi-repo feature is modeled as one backlog node per
project, linked by `blocked_by`, each shipping its own PR in its own repo:

1. `/blueprint` decomposes the feature into per-project nodes (`fno backlog decompose`), each with its own `project`/`cwd` and `plan_path` (or a `#fragment` of one shared design doc).
2. `/do` resolves each wave's project; a foreign, unblocked wave is dispatched via `fno agents spawn --cwd <root> "/target <node>"`; a foreign, still-blocked wave is deferred (carveout) to the merge trigger.
3. When a node's PR merges, `fno backlog advance` dispatches its now-unblocked cross-project dependents into their own projects.

A legacy plan carrying `scope: cross-project` is grandfathered: it warns and
routes to this model rather than running the removed pipeline.

## Work-Claim Coordination (`fno claim`)

Single coordination primitive for "this node/walker/fleet is being worked
on by someone." Subsumes the older PID-lock + `in_flight_nodes` filter +
graph-claim path. Key namespace is flat with typed prefixes:

- `node:<id>` - per-backlog-node target session claim
- `walker:<project_root>` - megawalk walker singleton
- `fleet:<mission_id>` - megatron commander singleton
- `project:<mission_id>:<project>` - per-project worker (reserved; not yet wired)
- `worktree:<path>` - reserved for future worktree-singleton claims

Files land at `.fno/claims/<url-encoded-key>.lock` with atomic
`O_CREAT|O_EXCL` semantics. Liveness is either PID (default; the holder's
process must be alive on this host with matching create_time) or TTL
(opt-in `expires_at`; refresh extends).

| Verb | Purpose |
|------|---------|
| `fno claim acquire <key> --holder ...` | Take a claim; idempotent re-acquire for same holder; stale recovery for dead holders |
| `fno claim release <key> --holder ...` | Drop a claim; silent no-op if not held |
| `fno claim refresh <key> --holder ...` | Extend a TTL claim (no-op for PID-liveness) |
| `fno claim status <key>` | Inspect one key: free/live/stale/corrupted + holder/pid/host |
| `fno claim list [--prefix p:]` | Enumerate claims, optionally `--include-stale` |
| `fno claim force-release <key> --reason ...` | Operator override; archives existing to `.expired/` |

PR1 (current) wires the primitive in alongside the legacy fields for
soak observability; PR2 will remove the legacy paths. See
[docs/architecture/coordination.md](docs/architecture/coordination.md)
for the full design.

## Agent Self-Introspection (`fno whoami` / `fno status`)

When confused about your operating context after compaction or a long
session, run `fno whoami` instead of grepping state files. Two read-only
top-level commands report the stacked context (fleet → walker → session):

| Verb | Purpose |
|------|---------|
| `fno whoami` | one-line summary of fleet + walker + session + provider |
| `fno status` | gate-by-gate satisfaction + bounded events tail + flagged inconsistencies |

These were formerly `fno agent whoami` / `fno agent status`; the `fno agent`
(singular) namespace was retired in (the never-auto-invoked
`suggest` / `capabilities` verbs were trimmed and the survivors relocated to
top-level). `fno agents` (plural, the dispatch mesh) is unrelated and untouched.

Distinct from `fno mail` (cross-project messaging between projects).
`fno whoami` / `fno status` are `man self` for the agent in its current
operating layer. Both are read-only - tests prove it with paired-state
md5 hashing across every read input. See `cli/README.md` for the full
surface.

## Install Health (`fno doctor`)

The deployed `fno` is a snapshot; a verb added to the repo after your last
install is invisible to it (PR #329's `backlog inbox` verb, since renamed to
`backlog capture`, is the canonical case: the `deferrals_captured` gate
depends on it). `fno doctor` detects that
skew, network-free, and exits non-zero only when staleness is proven:

| Flag | Purpose |
|------|---------|
| `fno doctor` | human verdict: `fresh` / `stale` / `unknown` (+ which Rust `fno-agents` binary `auto` mode resolves) |
| `fno doctor --json` | single stdout object `{status, python_stale, rust_stale, missing_verbs, source_rev, installed_rev, rust_binary, rust_installed_rev, rust_source_rev}` |
| `fno doctor --fix` | python-stale: delegates to `fno update` (whose Rust leg also refreshes the bins; keeps the `IN_PROGRESS` guard); rust-only-stale: runs the cargo refresh helper directly without a Python reinstall; under `--json`, repair is skipped with a skip message |

Three signals: a revision compare (`~/.fno/installed-rev`, written by `fno update` on success, vs the source's `git rev-parse HEAD`), a capability probe (`fno backlog capture --help`), and a Rust revision compare (`~/.fno/installed-rust-rev` vs the last commit touching `crates/`, degrading to `unknown` when any fact is missing). Each degrades to `unknown` rather than crying wolf. `fno update --rust` / `--no-rust` force or skip the Rust cargo leg explicitly. The `deferrals_captured` gate's strict-mode failure message is staleness-aware: a pre-#329 install gets an actionable `fno update` hint instead of a raw Typer error. The gate path instructs only; it never runs `fno update`. See [docs/architecture/installed-fno-staleness.md](docs/architecture/installed-fno-staleness.md).

## Looping Mechanisms

**In-session (stop hook).** `hooks/target-stop-hook.sh` is a read-only shim over `fno-agents loop-check`. It delegates all stop/allow decisions to the Rust verb, which checks: `<promise>` intent in session output, `done()` external reads (PR exists, CI green, every bot in `config.review.required_bots` reviewed with no unaddressed blocking inline finding - step 2), a backstop fingerprint (commit hash + events count), and budget caps. The `loop-check` DECISION writes no state and flips no gates. On a TERMINAL-allow decision the shim then invokes the separate `fno-agents finalize` WRITER (step 6) to re-home the mechanical session side-effects out of the skill's pre-promise sequence: always a ledger session-record (carrying `graph_node_id` + `provider_id` + `session_id` + `cost_usd` + `termination_reason`), and on a ship (`DonePRGreen`/`DoneAdvisory`) the plan stamp/graduate + handoff artifact. `finalize` is idempotent (`session_finalized` event guard) and strictly non-fatal (a failure emits `session_finalize_failed` and never blocks exit), so these records appear in every mode even when the agent compacted before pre-promise. Completion authority remains exactly three external reads + budget; nothing `finalize` writes is read by a future decision as a gate.

**Cross-session (the unified loop).** One Rust runtime (`fno-agents loop run`, step 5) drives all three loop altitudes; drivers differ only in their Queue/Dispatcher impls:

| Driver | Unit | Front door | Queue verbs shelled |
|---|---|---|---|
| `--driver target` | one session (degenerate walk) | `scripts/run-target-loop.sh` (exec shim) | none (manifest is the unit) |
| `--driver megawalk` | backlog node | `/megawalk` (launch-and-watch) | `fno backlog next` / `fno backlog done` |
| `--driver megatron --mission <id>` | fleet project (walked as a megawalk one altitude down) | `fno megatron run` (exec shim) | `fno megatron next` / `fno megatron complete` |

The walk stops on a `TerminationReason` event (DonePRGreen, DoneAdvisory, NoWork, Budget, NoProgress, or Interrupted) or the iteration ceiling. The legacy `fno loop` verb and the `/batch-queue` command surface are removed (backlog `rank` + `blocked_by` + `/megawalk` subsume the latter). See [docs/architecture/unified-loop.md](docs/architecture/unified-loop.md) for the runtime design, trait surface, and per-driver sections.

To signal distress without stopping the loop, emit `<help reason="..." evidence="...">...</help>`. Subprocess agents return `RESULT: BLOCKED` on stdout for orchestrators to parse - that is agent-to-orchestrator communication, not a session state write.

To cancel a session, `touch .fno/.target-cancelled` or export `TARGET_CANCEL=1`.

See [skills/target/references/failure-recovery.md](skills/target/references/failure-recovery.md) and [skills/target/references/state-schema.md](skills/target/references/state-schema.md).

## Iteration Loop

Skills that use bounded iteration share a common protocol:

`do ONE thing -> verify mechanically -> keep or discard -> repeat`

See `skills/target/references/iteration-loop.md`.

## Testing

```bash
# Run orchestrator CLI
python skills/do/orchestrator.py --help
python skills/do/orchestrator.py path/to/00-INDEX.md
python skills/do/orchestrator.py --agent "Build React component" --tags ui,frontend

# Validate test-first discipline
./scripts/validate-test-first.sh

# Analyze subagent metrics
./scripts/metrics/analyze.sh
```

## Skill/Agent Development

### Skill Structure
```
skills/<name>/
├── SKILL.md              # Main skill definition (YAML frontmatter + markdown)
├── references/           # Supporting docs
└── scripts/              # Shell scripts (optional)
```

### Agent Structure
```
agents/<name>.md          # Agent definition with frontmatter:
                          # - name, description, model, color
                          # - tools: [Read, Write, Edit, ...]
                          # - skills: [skill-names]
```

### Return Contract for Execution Agents

**Preferred (structured, the claude path).** Emit the result as a JSON object in a
fenced ```json block (or a `<result>{...}</result>` tag). It is validated at the
parse layer against the status enum, so a missing field or an invented status is
rejected rather than coerced - text conventions fail open; a schema validates
(ab-1394e797). Keys are case-insensitive:

```json
{"result": "SUCCESS", "task": "2.1", "commit": "abc123", "summary": "..."}
```

`result` must be exactly one of `SUCCESS | DONE_WITH_CONCERNS | FAILED | BLOCKED`;
`task` is required; `commit`/`summary`/`concerns`/`error`/`reason`/`unblocks_after`
are optional and carry the same meaning as the text grammar below.

**Fallback (text grammar, codex/gemini).** When a structured block is not emitted,
the `RESULT:` line grammar is parsed instead - but fail-closed: only these keys
are read as fields (appended prose is ignored), the first occurrence of each key
wins, and an out-of-enum status fails the parse rather than becoming `UNKNOWN`.

```
RESULT: SUCCESS|DONE_WITH_CONCERNS|FAILED|BLOCKED
TASK: task-id
COMMIT: hash (if SUCCESS/DONE_WITH_CONCERNS)
CONCERNS: what worries you (if DONE_WITH_CONCERNS)
ERROR: message (if FAILED)
REASON: why (if BLOCKED)
UNBLOCKS_AFTER: what needs to happen
```

The canonical parser is `parse_task_result` in `skills/do/orchestrator.py`
(structured-first, both paths enum-validated).

### Skill self-containment

Driver skills (`/target`, `/megawalk`, `/megatron`) are self-contained at the skill-folder level so each is portable to any markdown-aware runtime (Codex, Gemini, openclaw, hermes). The only external dependency is `fno`, declared in skill frontmatter as a binary dependency.

Four non-negotiable invariants enforced by CI:

- No `${REPO_ROOT}/scripts/` references (path doesn't exist outside the footnote repo)
- No `${SKILL_DIR}/../../scripts/` path escapes
- No `Skill()` runtime calls between driver skills (use Read for in-context disclosure, Task/Agent for subagents)
- No `../../_shared/` or `../../<sibling>/` path escapes

Cross-skill content reuse happens at BUILD TIME via `skill-bundles.yaml` + `scripts/generate-skill-bundles.sh`. Three content types: `files:` (cp), `references:` (frontmatter-strip), `agents:` (frontmatter-rewrite via `subagent_meta`).

Polished CLI: `fno bundle` (regenerate), `fno bundle check` (freshness gate), `fno bundle lint` (marketplace-readiness lint). The manifest itself (`skill-bundles.yaml`) documents each entry inline.

## Self-Improvement Loop

The autocorrect architecture replaces the deprecated feels system as the meta-improvement layer. See the self-improvement-loop design notes in the maintainers' vault for the architectural decision and implementation spec.

Capture is passive: git post-commit hooks on `~/.claude/`, pre-commit verifier hits, and tagged `/insights` write to `~/.claude/corrections.log`. A monthly cron + email script bundles the log delta + git log + current rule text and sends it to a fresh Claude API call for review; the dev triages the suggested patches in ~20 minutes.

### Memory pass

Two main-thread checkpoints capture session learnings into project-scoped memory:

- **Pre-promise** runs in target's pre-promise sequence (before `<promise>` emission). Scans for corrections, surprises, validated approaches, project facts.
- **Post-merge** triggers on `pr-merge.sh` success via `.fno/.memory-pass-pending`. Picks up late-arriving signal (human review comments, ungraduated sigma-review concerns).

Both call `scripts/memory/write-memory-entry.sh`. See [docs/architecture/memory-system.md](docs/architecture/memory-system.md).

### BLOCKED postmortems

When `fno-agents loop-check` emits a `TerminationReason` that signals a stuck session (NoProgress, Budget), the terminal-allow path writes a postmortem at `~/.fno/postmortems/{date}-{sid_short}.md`. The write lives in the `fno-agents finalize` WRITER (re-homed there in after the control-plane wedge turned the stop hook into a thin shim that drops the decision-only `loop-check` output): on a stuck terminal, finalize captures the session's last assistant message + recent git state into the artifact and best-effort appends a pointer line to `~/.claude/corrections.log` so the autocorrect monthly review consumes it. Non-fatal and idempotent like finalize's other side-effects (filename keyed by date+session). The corrections.log line is written only when that log already exists (the autocorrect feature creates it); finalize never creates it.

## Key Patterns

### Context Forking

Some skills use `context: fork` to run in isolated subprocesses with fresh context:

| Skill | Model | Rationale |
|-------|-------|-----------|
| `/pr create` | Haiku | Mechanical task - read commits, generate PR description |

Forked skills preserve main conversation context for complex work while offloading template-driven tasks.

### TDD Discipline
All execution agents enforce test-first:
1. Write failing test
2. Verify it fails (red)
3. Implement minimal code (green)
4. Verify database state (not just UI)
5. Atomic commit

### Deviation Rules
When encountering issues not in the plan:
- Bug in plan → Fix inline, note in SUMMARY.md
- Minor enhancement (<15 min) → Implement, note it
- Architecture decision → STOP, emit `<help reason="architecture-decision" evidence="...">` so the user can decide
- Missing dependency → STOP, emit `<help reason="missing-dependency" evidence="...">`

target-state.md is an immutable session manifest after init - the LLM must not write to it. To trigger a cancel, touch `.fno/.target-cancelled` or export `TARGET_CANCEL=1`. Subprocess agents may still return `RESULT: BLOCKED` on stdout for orchestrators to parse — that is agent-to-orchestrator communication, not a state file write.

### Promise Tags
Autonomous loops complete when output contains:
```
<promise>MISSION COMPLETE: all tasks done, tests passing, review feedback addressed, PR created</promise>
```

## Plugin Installation

```bash
# Development (recommended)
claude --plugin-dir /path/to/abilities

# Permanent
ln -s /path/to/abilities ~/.claude/plugins/abilities
```

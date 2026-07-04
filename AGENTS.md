# AGENTS.md

Project context and behavioral guidelines for AI agents (Claude Code, Gemini CLI, Codex CLI) in this repo. This file is the canonical source; `CLAUDE.md` and `GEMINI.md` are one-line stubs that `@AGENTS.md`-import it. It is a quick reference and an index: deep subsystem mechanics live in `docs/` (see [Deep-dive docs](#deep-dive-docs)) so this stays lean.

**footnote** is a Claude Code plugin: an autonomous delivery pipeline that takes a feature from idea to shipped PR (think → plan → do → review → ship). First time here? Run `fno setup wizard` (terminal) or `/fno:setup` (in-session) to write a validated `.fno/settings.yaml`. Defaults work, so `/fno:target "..."` runs without it.

## Working principles (Karpathy)

Bias toward caution over speed; for trivial tasks, use judgment. Derived from [Karpathy's notes](https://x.com/karpathy/status/2015883857489522876) on LLM coding pitfalls.

1. **Think before coding.** State assumptions; if uncertain, ask. Surface multiple interpretations and simpler alternatives instead of silently picking. Name what's confusing.
2. **Simplicity first.** Minimum code that solves the problem. No speculative features, single-use abstractions, unrequested config, or error handling for impossible cases. If 200 lines could be 50, rewrite.
3. **Surgical changes.** Touch only what the task requires. Don't refactor working code or restyle adjacent lines. Match existing style. Remove orphans *your* change created; flag pre-existing dead code, don't delete it. Every changed line should trace to the request.
4. **Goal-driven execution.** Turn tasks into verifiable goals ("add validation" → "write failing tests for invalid input, then make them pass"). State a brief plan with a verify step each. Strong success criteria let you loop independently.

## Repository

```
footnote/
├── .claude-plugin/   # Plugin manifest
├── skills/           # Skills; advertised set in skills/using-abilities/SKILL.md
├── agents/           # Subagents (target, code-reviewer, sigma-review specialists)
├── commands/         # Slash commands
├── hooks/            # Stop hooks, session-start, context monitor
├── scripts/          # Validation, metrics, orchestration, codemap, diagnostics
├── cli/              # The `fno` CLI (Python + uv) and its tests
├── crates/           # Rust runtime (fno-agents: loop-check, finalize, loop run)
└── internal ->       # Symlink to the Obsidian vault (plans/docs; not git-tracked)
```

### Conventions

- **Worktrees:** `claude --worktree <name>` is intercepted by footnote's `WorktreeCreate` hook (`hooks/worktree-setup.sh`); after creation, `bash scripts/setup/setup-worktree.sh` links shared state from canonical. Placement rule and full contract: [.claude/rules/worktrees.md](.claude/rules/worktrees.md).
- **Search hygiene:** prefer `rg` / the Grep tool over `grep -r` (which ignores `.gitignore` and descends into nested worktree checkouts, returning hundreds of false hits). If you must use `grep -r`, scope it to a path.
- **Multi-CLI:** skills are portable; orchestration needs per-CLI hook config. Gemini defaults to sequential execution; Codex uses `.codex/agents/`. Substrate facts in [docs/HARNESSES.md](docs/HARNESSES.md), wiring in [docs/architecture/multi-cli-hooks.md](docs/architecture/multi-cli-hooks.md), per-skill compat in [docs/SKILL-COMPAT-MATRIX.md](docs/SKILL-COMPAT-MATRIX.md). [RTK](https://github.com/rtk-ai/rtk) is a recommended companion for long loops (`/fno:setup` wires it).

## Commands

**Front door.** Six advertised verbs: `/target`, `/megawalk`, `/think`, `/review`, `/pr`, `/fix`. Each fans out to modes (`/review sigma|peer`, `/fix` + `investigate`, `/think` + `what-if|panel`, `/pr create|check|merged`, `/do flat|waves`). Everything else is invocable by full name. The advertised set lives in `skills/using-abilities/SKILL.md`, injected at SessionStart.

| Command | Purpose |
|---------|---------|
| `/target "feature"` | End-to-end: think → blueprint → do → review → ship |
| `/target path/to/plan` \| `/target <node-id>` | Execute an existing plan, or a backlog node by id (resolves via `~/.fno/graph.json`) |
| `/target L "feature"` | Large size: full ceremony including adversarial |
| `/target auto-merge "..."` | Auto-merge once external review passes (opt-in). [skills/_shared/auto-merge.md](skills/_shared/auto-merge.md) |
| `/megawalk` | Loop the ready backlog until done. `/megawalk roadmap <vision.md>` generates a backlog first |
| `/blueprint <doc-path>` | Mutate a design doc in place (Execution Strategy + File Ownership + kill_criteria). `quick "..."` for a flat single-file plan |
| `/do` | Execute a plan: `flat` (default) or `waves` |
| `/think` \| `/review` \| `/fix` \| `/tdd` \| `/triage` \| `/setup` | Design / review / fix-loop / TDD / spec-ordering / config wizard |
| `/pr create` \| `check` \| `merged` | Open a PR (Haiku worker) / poll+implement external review / post-merge ritual |

Surface evolution (one-liners; see linked docs): bare `/megawalk` replaced `continue`/`next`/`adopt --batch` ([megawalk-migration](skills/_shared/megawalk-migration.md)); `/blueprint` mutates the design doc in place rather than making a folder plan ([lean-blueprint](docs/architecture/lean-blueprint.md)); an approved native Plan-Mode plan is picked up by the next bare `/target`, which backfills the gates' required structure ([target-plan-mode-integration](docs/architecture/target-plan-mode-integration.md)).

## Backlog (`fno backlog`)

The feature graph lives under the `fno backlog` namespace (`fno graph` is a deprecated alias). **Day-to-day usage — creating, editing, moving cards between columns/swimlanes, lifecycle, the public roadmap — is in [docs/backlog-usage.md](docs/backlog-usage.md); the full verb list is there.** Essentials:

- **Node IDs:** `<prefix>-<hex>` (e.g. `fno-a3f9`); prefix/width set at `fno setup` (`config.backlog.id_prefix` / `id_hex_width`). Resolution is format-agnostic (any id resolves); generation is config-driven.
- **Slugs:** every node also has an immutable title-derived `slug` (`ab-1a2b3c4d` → `dashless-spawn`) that leads in display and is an accepted resolution input alongside the id, a bare hex, `next`, and a describe-it fuzzy match.
- **Lifecycle:** `intake → triage → ready/next → done`. Side states: `blocked` (open dependency) and `deferred` (paused via `defer`, reversible via `undefer`); `superseded` (replaced via `supersede`).
- **Priority:** `p0` drop-everything · `p1` next-up · `p2` normal (default) · `p3` long-tail. Orthogonal to `--size S|M|L`.
- **Editing:** `fno backlog update <id>` edits in place — `--details/--description` (rationale; `null` clears), `--domain`, `--size`, `--type`, `--priority`, `--public/--no-public`, etc. Use it instead of recreating via `idea` (which dupes).
- **Board == work order:** both boards order each column by `(project_lane, rank_band, priority, created_at)`; `fno backlog rank <id> --top` floats a card on the board *and* makes it run next. `_kanban_column` is the sole column authority; rank never changes a column. [backlog-board-ordering](docs/architecture/backlog-board-ordering.md).
- **Hygiene & automation** (all detailed in their docs): `fno backlog triage health [--check]` (metrics + thresholds); `fno backlog maintain [--apply]` (re-scope / leak-prune / auto-defer failure-prone #34, dedup/stale proposal-only); `fno backlog reconcile` (close nodes whose PR merged outside the gate; auto-fires on SessionStart); `fno backlog advance` (merge-triggered auto-continue, opt-in — [merge-triggered-auto-continue](docs/architecture/merge-triggered-auto-continue.md)).

## Execution & looping

**Waves + executors.** Plans declare waves in `00-INDEX.md`; `skills/do/orchestrator.py` routes tasks to specialized agents by keyword. Each task's executor resolves via a three-tier chain (task block → plan frontmatter → surface inference): `do`/`tdd` (archer, default) or `impeccable` (frontend-executor). Audit findings (a11y/perf/responsive/visual) gate independently from sigma-review. [executor-resolution](skills/do/references/executor-resolution.md).

**Looping.**
- *In-session:* `hooks/target-stop-hook.sh` is a read-only shim over `fno-agents loop-check`, which decides stop/allow from external truth only — `<promise>` intent, `done()` reads (PR exists, CI green, every `config.review.required_bots` bot reviewed with no unaddressed blocking inline finding), a backstop fingerprint, and budget. On terminal-allow it invokes `fno-agents finalize` (idempotent, non-fatal) for the ledger record + ship-time plan stamp.
- *Cross-session:* one Rust runtime, `fno-agents loop run`, drives both `--driver target` (one session) and `--driver megawalk` (backlog nodes via `fno backlog next`/`done`). Stops on a `TerminationReason` (DonePRGreen, DoneAdvisory, NoWork, Budget, NoProgress, Interrupted). [unified-loop](docs/architecture/unified-loop.md).
- Signal distress without stopping: `<help reason="..." evidence="...">...</help>`. Cancel: `touch .fno/.target-cancelled` or `export TARGET_CANCEL=1`. Subprocess agents return `RESULT: BLOCKED` on stdout (agent-to-orchestrator, not a state write).
- Shared iteration protocol: `do ONE thing → verify mechanically → keep or discard → repeat` ([iteration-loop](skills/target/references/iteration-loop.md)).

### State files & forbidden surfaces

NEVER edit these directly (a `PreToolUse` hook detects it). Use `fno backlog` / `fno state`:
- `~/.fno/graph.json` — the backlog graph. Mutate via `fno backlog` only (never Edit/Write/`jq -i`/`sed -i`).
- `.fno/target-state.md` — immutable session manifest after init. The only legal post-init write is first-fill of an empty `plan_path` via `fno state set`.

| File | Default | Purpose | Owner |
|------|---------|---------|-------|
| `paths.graph_json()` | `~/.fno/graph.json` (+ `.md` Kanban sibling) | Feature dependency graph | megawalk |
| `paths.ledger_json()` | `~/.fno/ledger.json` | Execution history + cost | target |
| `paths.briefs_dir()` | `~/.fno/briefs/{id}.md` | Sidecar discovery briefs | megawalk |
| `.fno/target-state.md` | project-relative | Immutable session manifest | target |
| `.fno/STATE.md` / `SUMMARY.md` / `00-INDEX.md` | project-relative | Wave progress / completion notes / execution strategy | /do, operator, /blueprint |
| `{plan_path}.artifacts/` | plan-relative | Quick-plan sidecar (COMPLETION.md, scratchpad-archive) | target stop hook |

Paths are resolved via `fno.paths`; override under `config.paths.*`. Check with `fno config doctor`; regenerate with `fno setup migrate-paths --force`. [path-config](docs/path-config.md).

### Ship vocabulary

"Ship" is overloaded. `/ship` (the verb) is the deliverable umbrella — `/ship pr` = `/pr` (the PR lifecycle), `/ship doc` ships a research brief. The **ship phase** is the `/target` step that creates the PR; the **ship gate** is where it stamps the plan frontmatter. Loop finish lines: `DonePRGreen` (code: PR + CI + reviewed) and `DoneAdvisory` (doc: written + eval-green). `/ship-docs` is the docs-generation skill (not a `/ship` type); `fno pr merge` is the merge primitive (not a ship type). [skills/ship/SKILL.md](skills/ship/SKILL.md).

### Plan completion stamp

At the ship gate `/target` stamps the plan frontmatter (`status: shipped|done`, `shipped_at`, `urls`, `session_ids`) — inline-list syntax only. `shipped` = first PR created (single-project → immediately `done`); `done` = all expected ships (cross-project: `len(urls) >= len(projects)`). [plan-completion-stamp](docs/architecture/plan-completion-stamp.md).

### Multi-repo features

No cross-project parallel-worktree pipeline: a session works only in its own project. A multi-repo feature is one backlog node per project, linked by `blocked_by`, each shipping its own PR. `/blueprint` decomposes (`fno backlog decompose`); `/do` spawns foreign unblocked waves via `fno agents spawn --cwd <root>`; on merge, `fno backlog advance` dispatches now-unblocked cross-project dependents. Legacy `scope: cross-project` plans warn and route here.

### Return contract for execution agents

Preferred (claude): a JSON object in a fenced ```json block (or `<result>{...}</result>`), validated against the status enum at parse time:

```json
{"result": "SUCCESS", "task": "2.1", "commit": "abc123", "summary": "..."}
```

`result` ∈ `SUCCESS | DONE_WITH_CONCERNS | FAILED | BLOCKED`; `task` required; `commit`/`summary`/`concerns`/`error`/`reason`/`unblocks_after` optional. Fallback (codex/gemini): the `RESULT:`/`TASK:`/`COMMIT:`/`CONCERNS:`/`ERROR:`/`REASON:`/`UNBLOCKS_AFTER:` line grammar, fail-closed (first occurrence wins, out-of-enum status fails the parse). Canonical parser: `parse_task_result` in `skills/do/orchestrator.py`.

### Deviation rules

Bug in plan → fix inline, note in SUMMARY.md. Minor enhancement (<15 min) → implement, note it. Architecture decision or missing dependency → STOP, emit `<help reason="..." evidence="...">` so the user decides.

## CLI subsystems (summary + doc)

- **`fno claim`** — the single work-claim primitive (`node:<id>`, `walker:<root>`, `fleet:<id>`); atomic lockfiles under `.fno/claims/`, PID or TTL liveness. `fno target init` already claims the node — never `fno claim acquire` manually. [coordination](docs/architecture/coordination.md).
- **`fno whoami` / `fno status`** — read-only self-introspection (fleet → walker → session); run when confused after compaction instead of grepping state. Distinct from `fno mail` (cross-project messaging).
- **`fno target start <node>`** — one-verb worktree cold-start for a bg `/target`: `fno worktree ensure` (off `origin/main`, never local HEAD) → heal `.fno` whole-dir symlink + link shared state → `fno target init` (claims the node once) → a parse-friendly receipt. Idempotent: a no-op from inside a worktree, never double-claims. Encodes the two silent killers (`.fno` symlink refusal, stale-base phantom-deletion PRs) that previously lived only in agent memory. [target-start-verb](docs/architecture/target-start-verb.md).
- **Spawn substrate axis** — `fno agents spawn --substrate <pane|bg|headless>` names one axis (where an off-thread worker runs): `pane` (owned-PTY drivable pane, the default), `bg` (detached `claude --bg` thread, claude-only; `/target bg` dispatches this), `headless` (one-shot `claude -p` / `codex --exec` / `agy -p`). The rule is **never default to claude `-p`**: `pane`/`bg` never shell `-p`; `-p` is reachable only via the explicit `headless` verb. `bg` on a non-claude provider is a hard error pointing to `headless`. `ask` and the relay claude hop keep `claude --bg`.
- **`fno doctor`** — detects when the deployed `fno` is stale vs the source (revision + capability + Rust-rev signals); `--fix` delegates to `fno update`. NOTE: it compares against the merged source, so it can't see unmerged local branches. [installed-fno-staleness](docs/architecture/installed-fno-staleness.md).
- **Provider rotation** — `fno providers` manages provider records, failover, per-model lockout, routing, combos. [provider-rotation](docs/provider-rotation.md). Cross-model review routes individual `/review sigma` agents to a different provider ([cross-model-review](docs/architecture/cross-model-review.md)); role-based routing sends auxiliary roles to a secondary provider via `--role` ([role-based-model-routing](docs/architecture/role-based-model-routing.md)).
- **Control-plane LOC ratchet** — a positive executable-LOC delta across control-plane paths (`hooks/`, `scripts/lib/`, verifiers, `cli/src/fno/loop.py`, `cli/src/fno/gates/`, gate_reality_map, `crates/.../loop*`) fails CI unless the PR body has a `loc-exception:` line AND a matching trajectory entry. [loc-ratchet](docs/architecture/loc-ratchet.md).
- **Post-merge ritual** — `/fno:pr merged` runs `reconcile` + `retro run`, then writes prose follow-ups to `config.post_merge.parking_lot_path` and files triage-worthy work. [auto-post-merge-ritual](docs/architecture/auto-post-merge-ritual.md).
- **Target self-handoff** — a `/target` session can hand the do phase (or a wave boundary at high context) to a fresh-context successor; generation-capped. [target-self-handoff](docs/architecture/target-self-handoff.md).
- **Self-improvement** — autocorrect (passive git-post-commit + verifier + `/insights` capture → monthly review) replaced the feels system. Two memory-pass checkpoints (pre-promise, post-merge) write project-scoped memory; stuck terminals write postmortems. [memory-system](docs/architecture/memory-system.md).

## Skill / agent development

- **Skill:** `skills/<name>/SKILL.md` (+ optional `references/`, `scripts/`). **Agent:** `agents/<name>.md` with frontmatter (name, description, model, tools, skills).
- **Self-containment (CI-enforced):** driver skills (`/target`, `/megawalk`) must be portable — no `${REPO_ROOT}/scripts/` refs, no `../../scripts/` or `../../<sibling>/` path escapes, no runtime `Skill()` calls between drivers. Cross-skill reuse happens at BUILD TIME via `skill-bundles.yaml` + `fno bundle` (`fno bundle check` gates freshness).
- **Context forking:** `/pr create` runs forked on Haiku (mechanical PR-description generation) to preserve main context.
- **TDD:** write failing test → red → minimal code → green → verify state → atomic commit.
- **Testing:** `python skills/do/orchestrator.py --help`; `./scripts/validate-test-first.sh`.

## Plugin installation

```bash
claude --plugin-dir /path/to/footnote          # development
ln -s /path/to/footnote ~/.claude/plugins/fno  # permanent
```

## Deep-dive docs

Backlog: [usage](docs/backlog-usage.md) · [board ordering](docs/architecture/backlog-board-ordering.md) · [triage](docs/backlog-triage.md) · [active dispatcher](docs/architecture/active-backlog-dispatcher.md) · [merge-triggered auto-continue](docs/architecture/merge-triggered-auto-continue.md)
Loop & target: [unified loop](docs/architecture/unified-loop.md) · [control-plane loop](docs/architecture/control-plane-loop.md) · [target reliability](docs/architecture/target-reliability-core.md) · [self-handoff](docs/architecture/target-self-handoff.md) · [plan-mode integration](docs/architecture/target-plan-mode-integration.md) · [loc-ratchet](docs/architecture/loc-ratchet.md)
Planning & ship: [lean blueprint](docs/architecture/lean-blueprint.md) · [plan completion stamp](docs/architecture/plan-completion-stamp.md) · [post-merge ritual](docs/architecture/auto-post-merge-ritual.md)
Coordination & providers: [coordination](docs/architecture/coordination.md) · [mail live-inject](docs/architecture/mail-live-inject.md) · [provider rotation](docs/provider-rotation.md) · [provider command matrix](docs/provider-command-matrix.md) · [cross-model review](docs/architecture/cross-model-review.md) · [role-based routing](docs/architecture/role-based-model-routing.md)
Platform & ops: [harnesses](docs/HARNESSES.md) · [multi-CLI hooks](docs/architecture/multi-cli-hooks.md) · [skill compat](docs/SKILL-COMPAT-MATRIX.md) · [path config](docs/path-config.md) · [installed-fno staleness](docs/architecture/installed-fno-staleness.md) · [memory system](docs/architecture/memory-system.md)

# AGENTS.md

Project context for AI agents (Claude Code, Gemini CLI, Codex CLI). Canonical source; `CLAUDE.md` / `GEMINI.md` are stubs that import it. Quick reference + index: deep subsystem mechanics live in `docs/` (see [Deep-dive docs](#deep-dive-docs)).

**footnote** is a Claude Code plugin: an autonomous delivery pipeline that takes a feature from idea to shipped PR (think -> plan -> do -> review -> ship). First time here? `fno setup wizard` (terminal) or `/fno:setup` (in-session). Defaults work without config.

## Working principles

0. **Worktree-first.** Whenever possible, enter a dedicated feature worktree before editing, generating, or committing (`worktree.policy = "never"` projects work in place by design). Keep the canonical main checkout unclogged. Prune after merge.
1. **Think before coding.** State assumptions; if uncertain, ask. Surface alternative interpretations and simpler options instead of silently picking.
2. **Simplicity first.** Minimum code that solves the problem. No speculative features, single-use abstractions, unrequested config. If 200 lines could be 50, rewrite.
3. **OSS-first: fix in the project, never memory-only.** Anything load-bearing (workaround, invariant, gotcha, "next time do X") goes in code, docs, `--help` text, a gate, a test, or a filed node - never private agent memory, which ships to nobody. Full rule: [.claude/rules/oss-fix-not-memory.md](.claude/rules/oss-fix-not-memory.md).
4. **Fix what you find - overrides "surgical changes."** Touch what the task requires and match existing style; that restraint is the only surgical constraint. Any pre-existing problem you discover (bug, flaky test, lint, dead code) gets FIXED in the same PR while context is warm, even when unrelated to the task. Prefer FEWER, larger PRs: batch the fix as its own atomic commit rather than splitting work across PRs. Carveouts (`fno carveout add`) / follow-up nodes are for genuinely large separable efforts only.
5. **Goal-driven execution.** Turn tasks into verifiable goals with a verify step each ("add validation" -> "write failing tests, make them pass").
6. **Comments earn their place.** Default to no comment; a good name beats one. Comment only non-obvious invariants, races, or why-not-the-obvious. Full rule: [.claude/rules/comments.md](.claude/rules/comments.md).
7. **Reproduce before you fix.** Reproduce a bug end-to-end on the real user path before editing; the repro is also the proof the fix landed. When a UI is in the loop, exercise it and be picky (see #4).
8. **Quality outweighs cost.** Weight quality, simplicity, robustness, and maintainability over effort-now. Never overrides #2.

## Pitfalls corpus (capped)

Hard-won traps a fresh agent re-hits because they are not yet a lint, guard, or refusal message. Inlined here (not a linked `.claude/rules/` file) because AGENTS.md is the one channel proven to reach every harness at session start: codex sees this inlined body but does not receive linked rule bodies, which auto-discover on Claude only. This is also the delivery target for the memory pass's lesson-candidate dual-emit, replacing the private-memory drain that codex cannot read and worktree workers never receive.

**Cap: 10 active entries (context-cost budget).** AGENTS.md is injected at every SessionStart on every harness, so every entry is paid on every session start on every lane. Do not raise it; an entry too large to fit its budget graduates to a lint. `scripts/ci/check-pitfalls.sh` fails CI on an 11th entry, a missing field, or an entry older than 60 days.

**Format:** one `###` block each. Imperative trap (1-3 sentences, this IS the budget), `specimens:` as bare file:line / PR refs, `graduates-to:` the lint/guard/refusal that lets it leave, `added:` YYYY-MM-DD. When a `graduates-to:` guard lands, remove the entry in the same PR that adds the guard (the guard is now the carrier, per principle 3's durability ladder).

AC9 delivery sentinel, echoed verbatim by a fresh worker with no file read to prove this corpus reached its harness; a lane that cannot has lost the delivery claim for it: `kdc-delivery-sentinel-1932`.

### A guard placed on one of N reachable paths is decorative

Before trusting a guard, enumerate every path a caller can reach (in-process test, exec'd binary, skill layer, direct CLI, spawned worker); a guard on only one reads as protection and ships green while the other paths stay broken. Behavior that lives only in skill prose is the same defect, since a direct CLI call or a non-Claude worker skips the skill layer and the rule never runs.

- specimens: `crates/fno/src/squad_store.rs:36` (`#[cfg(test)]` isolation protects in-process tests; the exec'd binary is `cfg(not(test))` and writes the live squads file, 124 orphaned squads with no surviving origin), `cli/tests/unit/test_pr_ritual.py` (`_bare()` constructs `Ritual` bypassing `__init__`, so the one wrong line shipped non-functional and green, PR #575 fixed by #577), `skills/agent/scripts/normalize.sh` (`--yolo` and slash-verb translation skipped by a direct `fno agents spawn`).
- graduates-to: the path-uniqueness lint that treats N reachable implementations of one operation as a CI failure, not a review catch.
- added: 2026-07-23

### Orienter output, claim snapshots, and liveness probes have all lied

Footnote's receipt lines, manifest snapshots, process argv, and liveness probes have each been caught lying about a live session; only the live lockfile and the transcript told the truth at every point. `fno target start` can print `plan: none` while a plan is bound, `node=already-claimed` while the claim is free, and `base=origin/main` while the branch is stale. Verify the load-bearing lines against source: `fno backlog get <id>` for status and plan, `fno claim status node:<id>` for the holder, `git rev-list --count HEAD..origin/main` for the real base, and the transcript mtime for liveness.

- specimens: `skills/target/SKILL.md` "Gotchas" (the receipt-can-lie cluster, and the rule that manifest claim fields are an init-time snapshot, not ownership truth).
- graduates-to: the receipt-truth contract (init auto-first-fills `plan_path`, prints the live claim holder, verifies the base) and transcript-keyed session liveness.
- added: 2026-07-23

### Judgment delegated to a subprocess on a truncated context produces junk

A subprocess seeing only a tail of structured signals makes wrong calls with full confidence; the deprecated distill path saw a 50-line tail and produced junk, which is why it was removed for cause. Keep all judgment (candidate selection, promotion, review) on full-context main threads; delegate only mechanical work to subprocesses.

- specimens: `docs/architecture/memory-system.md:77` (why Haiku distillation was deprecated for cause).
- graduates-to: a check that refuses to route a judgment call to a headless or bg subprocess.
- added: 2026-07-23

## Repository

```
footnote/
├── .claude-plugin/   # Plugin manifest
├── skills/           # Skills; advertised set in skills/using-fno/SKILL.md
├── agents/           # Subagents (target, code-reviewer, sigma-review specialists)
├── commands/         # Slash commands
├── hooks/            # Stop hooks, session-start, context monitor
├── scripts/          # Validation, metrics, orchestration, codemap, diagnostics
├── cli/              # The `fno` CLI (Python + uv) and its tests
├── crates/           # Rust runtime (fno-agents: loop-check, finalize, loop run)
└── internal ->       # Symlink to the Obsidian vault (plans/docs; not git-tracked)
```

### Conventions

- **Worktrees:** worktree-first for all repo work. `claude --worktree <name>` is intercepted by `hooks/worktree-setup.sh`; after creation run `bash scripts/setup/setup-worktree.sh`. Full contract: [.claude/rules/worktrees.md](.claude/rules/worktrees.md).
- **Search:** prefer `rg` / Grep over `grep -r` (which descends into nested worktrees). Scope any `grep -r` to a path.
- **Markdown prose:** one full sentence per physical line (semantic line breaks); never wrap a sentence across lines. Governs prose paragraphs, not bullets/fences/tables.
- **Multi-CLI:** skills are portable; orchestration needs per-CLI hook config. See [docs/HARNESSES.md](docs/HARNESSES.md), [docs/architecture/multi-cli-hooks.md](docs/architecture/multi-cli-hooks.md), [docs/SKILL-COMPAT-MATRIX.md](docs/SKILL-COMPAT-MATRIX.md).

## Commands

Six advertised verbs: `/fno:target`, `/fno:megawalk`, `/fno:think`, `/fno:review`, `/fno:pr`, `/fno:fix`, each fanning out to modes (`/fno:review sigma|peer`, `/fno:think what-if|panel`, `/fno:pr create|check|merged`, `/fno:do flat|waves`). Everything else stays invocable by full name; the advertised set lives in `skills/using-fno/SKILL.md`, injected at SessionStart. Always write verbs plugin-qualified (`/fno:...`) - a bare `/do` can resolve to another plugin.

| Command | Purpose |
|---------|---------|
| `/fno:target "feature"` | End-to-end: think -> blueprint -> do -> review -> ship |
| `/fno:target path/to/plan` \| `<node-id>` | Execute an existing plan or backlog node |
| `/fno:target L "feature"` | Large size: full ceremony including adversarial |
| `/fno:target auto-merge "..."` | Auto-merge once external review passes (opt-in). [auto-merge](skills/target/references/auto-merge.md) |
| `/fno:megawalk` | Loop the ready backlog until done. `roadmap <vision.md>` generates a backlog first |
| `/fno:blueprint <doc-path>` | Mutate a design doc in place; `quick "..."` for a flat single-file plan |
| `/fno:do` | Execute a plan: `flat` (default) or `waves` |
| `/fno:think` \| `/fno:review` \| `/fno:fix` \| `/fno:tdd` \| `/fno:triage` \| `/fno:setup` | Design / review / fix-loop / TDD / spec-ordering / config wizard |
| `/fno:pr create` \| `check` \| `merged` | Open PR (Haiku worker) / poll+implement external review / post-merge ritual |

Surface evolution: bare `/fno:megawalk` replaced `continue`/`next`/`adopt --batch` ([megawalk-migration](docs/architecture/megawalk-migration.md)); `/fno:blueprint` mutates the design doc in place ([lean-blueprint](docs/architecture/lean-blueprint.md)); an approved native Plan-Mode plan is picked up by the next bare `/fno:target` ([target-plan-mode-integration](docs/architecture/target-plan-mode-integration.md)).

## Backlog (`fno backlog`)

Day-to-day usage (create/edit/columns/lifecycle/roadmap) is in [docs/backlog-usage.md](docs/backlog-usage.md). Essentials:

- **Node IDs:** `<prefix>-<hex>` (e.g. `fno-a3f9`); generation config-driven, resolution format-agnostic. Every node also has an immutable `slug`; slugs, bare hex, `next`, and fuzzy matches all resolve.
- **Lifecycle:** `intake -> triage -> ready/next -> done`. Side states: `blocked`, `deferred` (`defer`/`undefer`), `superseded`.
- **Priority:** `p0`..`p3` (default `p2`); orthogonal to `--size S|M|L`.
- **Editing:** `fno backlog update <id>` in place (`--details`, `--domain`, `--size`, `--priority`, ...). Never recreate via `idea` (dupes).
- **Board == work order:** columns order by `(project_lane, rank_band, priority, created_at)`; `rank <id> --top` floats a card and makes it run next; `_kanban_column` is the sole column authority. [backlog-board-ordering](docs/architecture/backlog-board-ordering.md).
- **Hygiene:** `fno backlog groom` (daily pass), `triage health [--check]`, `maintain [--apply]`, `reconcile` (auto-fires on SessionStart), `advance` (merge-triggered auto-continue, opt-in).

## Execution & looping

**Waves + executors.** Plans declare waves in `00-INDEX.md`; `skills/do/orchestrator.py` routes tasks to agents by keyword. Executor resolves via task block -> plan frontmatter -> surface inference: `do`/`tdd` (archer, default) or `impeccable` (frontend-executor). [executor-resolution](skills/do/references/executor-resolution.md).

**Looping.**
- *In-session:* `hooks/target-stop-hook.sh` shims `fno-agents loop-check`, which decides stop/allow from external truth only: `<promise>` intent, done() reads (PR exists, CI green, every `config.review.required_bots` bot reviewed with no unaddressed blocking finding), any plan-declared `done_probes`, a backstop fingerprint, and budget. Terminal-allow invokes `fno-agents finalize` (idempotent).
- *Cross-session:* `fno-agents loop run` drives `--driver target` and `--driver megawalk`, stopping on a `TerminationReason` (DonePRGreen, DoneAdvisory, NoWork, Budget, NoProgress, Interrupted). [unified-loop](docs/architecture/unified-loop.md).
- Signal distress without stopping: `<help reason="..." evidence="...">...</help>`. Cancel: `touch .fno/.target-cancelled` or `TARGET_CANCEL=1`. Subprocess agents return `RESULT: BLOCKED` on stdout.
- Shared iteration protocol: do ONE thing -> verify mechanically -> keep or discard -> repeat ([iteration-loop](skills/target/references/iteration-loop.md)).

### State files & forbidden surfaces

NEVER edit these directly (a `PreToolUse` hook detects it). Use `fno backlog` / `fno state`:
- `~/.fno/graph.json` - the backlog graph; mutate via `fno backlog` only.
- `.fno/target-state.md` - immutable session manifest after init; only legal post-init write is first-fill of empty `plan_path` via `fno state set`.

| File | Default | Purpose | Owner |
|------|---------|---------|-------|
| `paths.graph_json()` | `~/.fno/graph.json` (+ `.md` Kanban) | Feature dependency graph | megawalk |
| `paths.ledger_json()` | `~/.fno/ledger.json` | Execution history + cost | target |
| `paths.briefs_dir()` | `~/.fno/briefs/{id}.md` | Sidecar discovery briefs | megawalk |
| `.fno/target-state.md` | project-relative | Immutable session manifest | target |
| `.fno/STATE.md` / `SUMMARY.md` / `00-INDEX.md` | project-relative | Wave progress / completion / strategy | /do, operator, /blueprint |
| `{plan_path}.artifacts/` | plan-relative | Quick-plan sidecar | target stop hook |

Paths resolve via `fno.paths`; override under `config.paths.*`; check with `fno config doctor`. [path-config](docs/path-config.md).

### Ship vocabulary

`/ship` is the deliverable umbrella (`/ship pr` = `/pr`; `/ship doc` ships a research brief). The **ship phase** is the `/target` step that creates the PR; the **ship gate** stamps plan frontmatter. Loop finish lines: `DonePRGreen` (PR + CI + reviewed) and `DoneAdvisory` (doc written + eval-green). `fno pr merge` is the merge primitive. [skills/ship/SKILL.md](skills/ship/SKILL.md).

### Plan completion stamp

At the ship gate `/target` stamps plan frontmatter (`status: in_review|done`, `shipped_at`, `urls`, `session_ids`) - inline-list syntax only. `in_review` = first PR created; `done` = all expected ships. [plan-completion-stamp](docs/architecture/plan-completion-stamp.md).

### Multi-repo features

A session works only in its own project. A multi-repo feature is one backlog node per project linked by `blocked_by`, each shipping its own PR: `/blueprint` decomposes, `/do` spawns foreign unblocked waves via `fno agents spawn --cwd <root>`, `fno backlog advance` dispatches dependents on merge.

### Return contract for execution agents

Preferred (claude): a JSON object in a fenced ```json block (or `<result>{...}</result>`):

```json
{"result": "SUCCESS", "task": "2.1", "commit": "abc123", "summary": "..."}
```

`result` ∈ `SUCCESS | DONE_WITH_CONCERNS | FAILED | BLOCKED`; `task` required. Fallback (codex/gemini): the `RESULT:`/`TASK:`/... line grammar, fail-closed. Canonical parser: `parse_task_result` in `skills/do/orchestrator.py`.

### Deviation rules

Bug in plan -> fix inline, note in SUMMARY.md. Minor enhancement (<15 min) -> implement, note it. Architecture decision or missing dependency -> STOP, emit `<help>`. Under a beastmode grant (`authority: full`) that last rule inverts: decide, append to the `## Autonomous Decisions` ledger, continue; genuine blockers still stop. [skills/target/SKILL.md](skills/target/SKILL.md#authority-the-beastmode-grant).

## CLI subsystems (summary + doc)

- **`fno claim`** - the single work-claim primitive; atomic lockfiles under `.fno/claims/`. `fno target init` already claims the node - never `fno claim acquire` manually. [coordination](docs/architecture/coordination.md).
- **`fno whoami` / `fno status`** - read-only self-introspection; run when confused after compaction.
- **`fno target start <node>`** - one-verb worktree cold-start (worktree ensure off `origin/main` -> heal `.fno` symlink -> `fno target init`), idempotent. [target-start-verb](docs/architecture/target-start-verb.md).
- **Spawn substrate axis** - `fno agents spawn --substrate <pane|bg|headless>`: `pane` (default), `bg` (`claude --bg`, claude-only), `headless` (one-shot `-p`/`--exec`). Never default to `-p`; it is reachable only via explicit `headless`.
- **`fno doctor`** - detects stale deployed `fno` vs source; `--fix` delegates to `fno update`. Compares against merged source only. [installed-fno-staleness](docs/architecture/installed-fno-staleness.md).
- **Provider rotation** - `fno providers`: records, failover, lockout, routing, combos. [provider-rotation](docs/provider-rotation.md) · [cross-model-review](docs/architecture/cross-model-review.md) · [role-based routing](docs/architecture/role-based-model-routing.md).
- **Curated CLI menu** - `fno --help` shows ~9 verbs; most commands are hidden but invocable. `fno help --all` / `fno help <group> --all` list everything. New verbs default hidden; `fno lint menu-caps` gates the advertised surface (10 top-level / 12 per sub-app).
- **Control-plane LOC ratchet** - positive executable-LOC delta across control-plane paths fails CI unless the PR body has a `loc-exception:` line AND a matching trajectory entry. [loc-ratchet](docs/architecture/loc-ratchet.md).
- **Post-merge ritual** - `/fno:pr merged` runs reconcile + retro, writes follow-ups to `config.post_merge.parking_lot_path`. [auto-post-merge-ritual](docs/architecture/auto-post-merge-ritual.md).
- **Target self-handoff** - a `/target` session can hand the do phase to a fresh-context successor; generation-capped. [target-self-handoff](docs/architecture/target-self-handoff.md).
- **Self-improvement** - autocorrect (git-post-commit + verifier + `/insights` -> monthly review); two memory-pass checkpoints; stuck terminals write postmortems. [memory-system](docs/architecture/memory-system.md).

## Skill / agent development

- **Skill:** `skills/<name>/SKILL.md` (+ optional `references/`, `scripts/`). **Agent:** `agents/<name>.md` with frontmatter.
- **Self-containment (CI-enforced):** driver skills (`/target`, `/megawalk`) must be portable - no `${REPO_ROOT}/scripts/` refs, no path escapes, no runtime `Skill()` calls between drivers. Cross-skill reuse happens at build time via `skill-bundles.yaml` + `fno bundle` (`fno bundle check` gates freshness).
- **TDD:** failing test -> red -> minimal code -> green -> verify -> atomic commit.
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
Coordination & providers: [coordination](docs/architecture/coordination.md) · [mail live-inject](docs/architecture/mail-live-inject.md) · [provider rotation](docs/provider-rotation.md) · [harness command matrix](docs/harness-command-matrix.md) · [cross-model review](docs/architecture/cross-model-review.md) · [role-based routing](docs/architecture/role-based-model-routing.md)
Platform & ops: [harnesses](docs/HARNESSES.md) · [multi-CLI hooks](docs/architecture/multi-cli-hooks.md) · [skill compat](docs/SKILL-COMPAT-MATRIX.md) · [path config](docs/path-config.md) · [installed-fno staleness](docs/architecture/installed-fno-staleness.md) · [memory system](docs/architecture/memory-system.md)

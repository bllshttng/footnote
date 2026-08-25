# AGENTS.md

Project context for AI agents (Claude Code, Gemini CLI, Codex CLI). Canonical source; `CLAUDE.md` / `GEMINI.md` are stubs that import it. Quick reference + index: deep subsystem mechanics live in `docs/` (see [Deep-dive docs](#deep-dive-docs)).

**footnote** is a Claude Code plugin: an autonomous delivery pipeline that takes a feature from idea to shipped PR (think -> plan -> do -> review -> ship). First time here? `fno config setup wizard` (terminal) or `/fno:setup` (in-session). Defaults work without config.

## Precedence and output style

Generic per-machine coding skills (ponytail, karpathy-guidelines, similar) are advisory here; this file's principles win. Two live cases: "shortest diff" loses to principle 4 (fix what you find, in this PR), and tool-branded comments (`// ponytail:`) are barred by the comment principle.

Lead responses with the next action, number multi-step work, give concrete time estimates, and drop preamble, recaps, and closers. Details: [docs/output-style.md](docs/output-style.md).

## Working principles

0. **Worktree-first.** Whenever possible, enter a dedicated feature worktree before editing, generating, or committing (`worktree.policy = "never"` projects work in place by design). Keep the canonical main checkout unclogged. Prune after merge.
1. **Think before coding.** State assumptions; if uncertain, ask. Surface alternative interpretations and simpler options instead of silently picking.
2. **Simplicity first.** Minimum code that solves the problem. No speculative features, single-use abstractions, unrequested config. If 200 lines could be 50, rewrite.
3. **OSS-first: fix in the project, never memory-only.** Anything load-bearing (workaround, invariant, gotcha, "next time do X") goes in code, docs, `--help` text, a gate, a test, or a filed node - never private agent memory, which ships to nobody. Full rule: [.claude/rules/oss-fix-not-memory.md](.claude/rules/oss-fix-not-memory.md).
4. **Fix what you find - overrides "surgical changes."** Fix pre-existing problems in this PR, even when unrelated. Prefer FEWER, larger PRs. Carveouts (`fno backlog carveout add`) / follow-up nodes are for genuinely large separable efforts only.
5. **Goal-driven execution.** Turn tasks into verifiable goals with a verify step each ("add validation" -> "write failing tests, make them pass").
6. **Comments earn their place.** Match the surrounding file's comment density and idiom; add one only for a non-obvious invariant, race, or why-not-the-obvious. Never ticket/PR/node IDs (`scripts/ci/check-no-internal-refs.sh` fails on them).
7. **Reproduce before you fix.** Reproduce a bug end-to-end on the real user path before editing; the repro is also the proof the fix landed. When a UI is in the loop, exercise it and be picky (see #4).
8. **Quality outweighs cost.** Weight quality, simplicity, robustness, and maintainability over effort-now. Never overrides #2.

## Pitfalls corpus (capped)

Traps a fresh agent re-hits because they are not yet a lint, guard or refusal. Inlined, not linked: AGENTS.md is the one channel proven to reach every harness at session start, and codex sees this body, not linked rule bodies.

**Cap: bytes, not count.** Every entry is paid at session start. `check-pitfalls.sh` fails on an 11th entry, a missing field or one over 60 days. The byte budget binds first near 5. This prose is at its floor; funding growth is unsolved.

**Format:** one `###` block each: imperative trap (1-3 sentences), `specimens:` file:line refs, `graduates-to:` the guard that retires it, `added:` YYYY-MM-DD. Remove an entry in the PR where its guard lands.

AC9 delivery sentinel, echoed verbatim by a fresh worker with no file read, proving this corpus reached its harness. A unit test asserts it: `kdc-delivery-sentinel-1932`.

### Orienter output, claim snapshots, and liveness probes have all lied

Receipts, manifest snapshots, process argv and liveness probes have each lied about a live session. Only the live lockfile and the transcript stayed truthful. `fno do target start` can print `plan: none` with a plan bound, or `base=origin/main` on a stale branch. Verify load-bearing lines against source: `fno backlog get <id>` (status/plan), `fno agents claim status node:<id>` (holder), `git fetch origin main && git rev-list --count HEAD..origin/main` (real base; skip the fetch and a stale ref answers 0).

- specimens: `skills/target/SKILL.md` "Gotchas" (the receipt-can-lie cluster; manifest claim fields are an init-time snapshot, not ownership truth).
- graduates-to: the receipt-truth contract (init first-fills `plan_path`, prints the live holder, verifies the base) and transcript-keyed liveness.
- added: 2026-07-23

### Assert a positive marker, never an absence

An absence has three explanations: the real outcome, "the instrument never ran", or a pipeline loss. In the third case, the instrument ran, it HIT, and the pipeline ate the output before anything read it. Require a marker only the outcome produces, pinned to the measured thing, not a matching word. A positive control validates the TOOL, not the TARGET. A green control aimed at the wrong SYMBOL still reads as proof. Before trusting a zero, name the behavior's symbol: for a Python capability, use the function name, not the CLI spelling. Never truncate or post-process a search whose zero you intend to trust.

- specimens: 2026-08-23. Repo-wide grep found `manifest-path`. `head -6` buried it behind a JSON fixture. It produced a wrong p1 root cause. `grep -c` through rtk returned 0 on a read diff containing the string. Quoted-token regex missed bare tokens in a whitespace-split docstring. It nearly closed a live node as resolved-elsewhere.
- graduates-to: An assert helper rejects absence-only success and zero-hit probes without a positive control. Honest exit codes answering the wrong question still need a verdict verb.
- added: 2026-07-27

### A capability probe delivered over the mail bus can only ever return yes

`fno agents mail send` injects as user-shaped text, indistinguishable from operator typing. So a "can the agent do X unprompted?" probe sent by mail tests the USER-TRIGGERED path and cannot fail. Reading that as proof of autonomy is the receipt-can-lie shape: a snapshot that a call was accepted, not that an agent can make it unaided. The valid test is a run with no user-shaped prompt in the transcript.

- specimens: 2026-08-05, a `/code-review` probe mailed to a worker succeeded and was read as proof of self-invocation; the mail was the user-shaped trigger.
- graduates-to: a probe distinguishing user-shaped injection from an autonomous tool call, or a lint flagging a capability claim evidenced only by a mail probe.
- added: 2026-08-05

### Codex RPC

`fno agents mail send <full-session-id> --raw '/review'` or `'/code-review'` fires daemon RPC. Other verbs: `fno mux pane send --raw --submit`. Read pane for silent verbs.

- specimens: mail_inject.rs, mux_cli.rs
- graduates-to: submit receipt
- added: 2026-08-24

## Repository

```
footnote/
├── .claude-plugin/   # Plugin manifest
├── skills/           # Skills (advertised set in using-fno)
├── agents/           # Subagents (target, code-reviewer, sigma-review)
├── commands/         # Slash commands
├── hooks/            # Stop hooks, session-start, context monitor
├── scripts/          # Validation, metrics, orchestration, diagnostics
├── cli/              # `fno` CLI (Python + uv) + tests
├── crates/           # Rust runtime (fno-agents)
└── internal ->       # Obsidian vault symlink (plans/docs; not git-tracked)
```

### Conventions

- **Worktrees:** worktree-first for all repo work. `claude --worktree <name>` is intercepted by `hooks/worktree-setup.sh`; after creation run `bash scripts/setup/setup-worktree.sh`. Full contract: [.claude/rules/worktrees.md](.claude/rules/worktrees.md).
- **Search:** prefer `rg` / Grep over `grep -r` (which descends into nested worktrees); scope any `grep -r` to a path. For a load-bearing sweep use `RIPGREP_CONFIG_PATH= rg -uu`, not a bare `rg -uu` (`-u` ignores files, not globs); the over-exclusion trap is a pitfalls entry above.
- **Prose style:** a paragraph is ONE physical line. A newline starts the next block. House style, and the gate: [docs/style-rules.md](docs/style-rules.md).
- **Multi-CLI:** skills are portable; orchestration needs per-CLI hook config. See [docs/HARNESSES.md](docs/HARNESSES.md), [docs/architecture/multi-cli-hooks.md](docs/architecture/multi-cli-hooks.md), [docs/SKILL-COMPAT-MATRIX.md](docs/SKILL-COMPAT-MATRIX.md).

## Commands

Five advertised verbs: `/fno:target`, `/fno:think`, `/fno:review`, `/fno:pr`, `/fno:fix`, each fanning out to modes (table below). Everything else stays invocable by full name. The advertised set lives in `skills/using-fno/SKILL.md`. Always write verbs plugin-qualified (`/fno:...`) - a bare `/execute` can resolve to another plugin.

| Command | Purpose |
|---------|---------|
| `/fno:target "feature"` | End-to-end: think -> blueprint -> do -> review -> ship |
| `/fno:target path/to/plan` \| `<node-id>` | Execute an existing plan or backlog node |
| `/fno:target L "feature"` | Large size: full ceremony including adversarial |
| `/fno:target auto-merge "..."` | Auto-merge once external review passes (opt-in). [auto-merge](skills/target/references/auto-merge.md) |
| `/fno:blueprint <doc-path>` | Mutate a design doc in place; `quick "..."` for a flat single-file plan |
| `/fno:execute` | Execute a plan: `flat` (default) or `waves` |
| `/fno:think` \| `/fno:review` \| `/fno:fix` \| `/fno:tdd` \| `/fno:triage` \| `/fno:setup` | Research / review / fix-loop / TDD / spec-ordering / config wizard |
| `/fno:pr create` \| `check` \| `merged` | Open PR (pr-create role worker) / poll+implement external review / post-merge ritual |
| `/fno:growth-launch "<objective>"` | Growth-studio pack: four-role campaign bundle held at a founder approval gate |

Surface evolution: `/fno:blueprint` mutates the design doc in place ([lean-blueprint](docs/architecture/lean-blueprint.md)); an approved native Plan-Mode plan is picked up by the next bare `/fno:target` ([target-plan-mode-integration](docs/architecture/target-plan-mode-integration.md)).

## Backlog (`fno backlog`)

Day-to-day usage (create/edit/columns/lifecycle/roadmap) is in [docs/backlog-usage.md](docs/backlog-usage.md). Essentials:

- **Node IDs:** `<prefix>-<hex>` (e.g. `fno-a3f9`); generation config-driven, resolution format-agnostic. Every node also has an immutable `slug`; slugs, bare hex, `next`, and fuzzy matches all resolve.
- **Lifecycle:** `intake -> triage -> ready/next -> done`. Side states: `blocked`, `deferred` (`defer`/`undefer`), `superseded`.
- **Priority:** `p0`..`p3` (default `p2`); orthogonal to `--size S|M|L`.
- **Editing:** `fno backlog update <id>` in place (`--details`, `--domain`, `--size`, `--priority`, ...). Never recreate via `idea` (dupes).
- **Board == work order:** non-Done cards share a rank suffix (live-epic children before epics, then priority, then created_at); project lane is a board-only display prefix; `rank <id> --top` floats a card and makes it run next; `_kanban_column` is the sole column authority. [backlog-board-ordering](docs/architecture/backlog-board-ordering.md).
- **Hygiene:** `fno backlog groom` (daily pass), `triage health [--check]`, `maintain [--apply]`, `reconcile` (auto-fires on SessionStart), `advance` (merge-triggered auto-continue, opt-in).

## Execution & looping

**Waves + executors.** Plans declare waves in `00-INDEX.md`; `skills/execute/orchestrator.py` routes tasks to agents by keyword. Executor resolves via task block -> plan frontmatter -> surface inference: `do`/`tdd` (archer, default) or `impeccable` (frontend-executor). [executor-resolution](skills/execute/references/executor-resolution.md).

**Looping.**
- *In-session:* `hooks/target-stop-hook.sh` shims `fno-agents loop-check`, which decides stop/allow from external truth only: `<promise>` intent, done() reads (PR exists, CI green, every `config.review.required_bots` bot reviewed with no unaddressed blocking finding), any plan-declared `done_probes`, a backstop fingerprint, and budget. Terminal-allow invokes `fno-agents finalize` (idempotent).
- *Cross-session:* `fno-agents loop run` drives `--driver target`, stopping on a `TerminationReason` (DonePRGreen, DoneAdvisory, DoneDelivery, NoWork, Budget, NoProgress, Interrupted). [unified-loop](docs/architecture/unified-loop.md).
- Signal distress without stopping: `<help reason="..." evidence="...">...</help>`. Cancel: `touch .fno/.target-cancelled` or `TARGET_CANCEL=1`. Subprocess agents return `RESULT: BLOCKED` on stdout.
- Shared iteration protocol: do ONE thing -> verify mechanically -> keep or discard -> repeat ([iteration-loop](skills/target/references/iteration-loop.md)).

### State files & forbidden surfaces

NEVER edit these directly (a `PreToolUse` hook detects it). Use `fno backlog` / `fno do state`:
- `~/.fno/graph.json` - the backlog graph; mutate via `fno backlog` only.
- `.fno/target-state.md` - immutable session manifest after init; only legal post-init write is first-fill of empty `plan_path` via `fno do state set`.

| File | Default | Purpose | Owner |
|------|---------|---------|-------|
| `paths.graph_json()` | `~/.fno/graph.json` (+ `.md` Kanban) | Feature dependency graph | backlog |
| `paths.ledger_json()` | `~/.fno/ledger.json` | Execution history + cost | target |
| `paths.briefs_dir()` | `~/.fno/briefs/{id}.md` | Sidecar discovery briefs | backlog |
| `.fno/target-state.md` | project-relative | Immutable session manifest | target |
| `.fno/STATE.md` / `SUMMARY.md` / `00-INDEX.md` | project-relative | Wave progress / completion / strategy | /execute, operator, /blueprint |
| `{plan_path}.artifacts/` | plan-relative | Quick-plan sidecar | target stop hook |

Paths resolve via `fno.paths`; override under `config.paths.*`; check with `fno config doctor`. [path-config](docs/path-config.md). A state-root TOP-LEVEL write needs an owner + lifetime in [state-root-inventory](docs/state-root-inventory.md); session-keyed files go in a subfolder.

### Ship vocabulary

`/ship` is the deliverable umbrella (`/ship pr` = `/pr`; `/ship doc` ships a research brief). The **ship phase** is the `/target` step that creates the PR; the **ship gate** stamps plan frontmatter. Loop finish lines: `DonePRGreen` (PR + CI + reviewed), `DoneUnreviewed` (green, unreviewed), `DoneAdvisory` (doc + eval-green), `DoneDelivery` (current evidence). `fno do pr merge` is the merge primitive. [skills/ship/SKILL.md](skills/ship/SKILL.md).

### Plan completion stamp

At the ship gate `/target` stamps plan frontmatter (`status: in_review|done`, `shipped_at`, `urls`, `session_ids`). `in_review` = first PR created; `done` = all expected ships. Node closure also clears a **promise gate** (exit 6).

### Multi-repo features

A session works only in its own project. A multi-repo feature is one node per project linked by `blocked_by`, each shipping its own PR: `/blueprint` decomposes, `/execute` spawns foreign unblocked waves via `fno agents spawn --cwd <root>`, `fno backlog advance` dispatches dependents on merge.

### Return contract for execution agents

Preferred (claude): a JSON object in a fenced ```json block (or `<result>{...}</result>`):

```json
{"result": "SUCCESS", "task": "2.1", "commit": "abc123", "summary": "..."}
```

`result` ∈ `SUCCESS | DONE_WITH_CONCERNS | FAILED | BLOCKED`; `task` required. Fallback (codex/gemini): the `RESULT:`/`TASK:`/... line grammar, fail-closed. Parser: `parse_task_result` in `skills/execute/orchestrator.py`.

### Deviation rules

Bug in plan -> fix inline, note in SUMMARY.md. Minor enhancement (<15 min) -> implement, note it. Architecture decision or missing dependency -> STOP, emit `<help>`. Under a beastmode grant (`authority: full`) that last rule inverts: decide, append to the `## Autonomous Decisions` ledger, continue; genuine blockers still stop. [skills/target/SKILL.md](skills/target/SKILL.md#authority-the-beastmode-grant).

## CLI subsystems (summary + doc)

- **`fno agents claim`** - the one work-claim primitive; atomic lockfiles under `.fno/claims/`. `target init` already claims the node - never `claim acquire` manually. [coordination](docs/architecture/coordination.md).
- **`fno agents mail` - king-mediated native review.** A worker self-invokes the native review verb (claude `/code-review`, codex `/review`) via the Skill tool first; when refused, `fno agents mail send <worker> --raw '/<verb>'` fires it at the prompt line (a wrapped reply won't). The code-payload self-review obligation is enforced at the stop gate (`loopcheck.rs`) and `fno do pr merge`; opt out `config.review.self_review_required = false`. [review lanes](docs/architecture/review-lanes.md).
- **`fno backlog decide`** - records a ruling per subject. `fno backlog decisions X` recovers it, newest first. [decision-record](docs/architecture/decision-record.md).
- **`fno whoami` / `fno whoami status`** - read-only self-introspection; run when confused after compaction.
- **`fno do target start <node>`** - one-verb worktree cold-start (ensure off `origin/main` -> heal `.fno` symlink -> `target init`), idempotent. [target-start-verb](docs/architecture/target-start-verb.md).
- **Spawn substrate axis** - `fno agents spawn --substrate <pane|bg|headless>`: `pane` (default), `bg` (claude `--bg`, opencode serve lane), `headless` (one-shot `-p`/`--exec`). `-p` is reachable only via explicit `headless`; never default to it.
- **`fno agents watchdog`** - fleet sweep from transcript truth: wake / reroute / reap. Dry run by default; `--apply` (wake), `--apply-all` (reroute; reap needs `config.recovery.watchdog_reap`, it deletes worktrees). Cadence behind `config.recovery.watchdog`. [fleet-watchdog](docs/architecture/fleet-watchdog.md)
- **`fno doctor`** - detects stale deployed `fno` vs merged source only. `--fix` delegates to `fno doctor update`. [installed-fno-staleness](docs/architecture/installed-fno-staleness.md).
- **`fno doctor footprint`** - measures sustained fleet CPU and process count without load average; see [machine footprint](docs/architecture/machine-footprint.md).
- **Accounts + rotation** - `fno config accounts`: records, failover, lockout, routing, combos. Five axes, never confuse them: harness (`-H`), provider (vendor, `-P`), model (`-m`), effort (`--effort`), account (`--account`). `opencode` is legally both harness and provider. Never infer the axis from a value. Definitions live in [axis-vocabulary](docs/architecture/axis-vocabulary.md).
- **Stage table (per-stage axis)** - `config.agents.profiles.<verb>` overlays `agents.defaults`, reaches autonomous dispatch; `route`=vendor/model (`--route`, fail-closed) beside `provider`=harness. [stage table](docs/architecture/role-based-model-routing.md).
- **Curated CLI menu** - `fno --help` shows ~9 verbs. Most commands are hidden but invocable. `fno help --all` lists everything (`help <group> --all` per group). New verbs default hidden. `fno doctor lint menu-caps` gates the advertised surface (10 top-level / 12 per sub-app). Group actions are arguments, not leaves.
- **Post-merge ritual** - `/fno:pr merged` runs reconcile + retro; follow-ups go to `config.post_merge.parking_lot_path`.
- **Target self-handoff** - `/target` can hand the do phase to a fresh-context successor; generation-capped. [target-self-handoff](docs/architecture/target-self-handoff.md).
- **Self-improvement** - autocorrect (git-post-commit + verifier + `/insights` -> monthly review); two memory-pass checkpoints; stuck terminals write postmortems. [memory-system](docs/architecture/memory-system.md).

## Skill / agent development

- **Skill:** `skills/<name>/SKILL.md` (+ optional `references/`, `scripts/`). **Agent:** `agents/<name>.md` with frontmatter.
- **Self-containment (CI-enforced):** driver skills (`/target`) must be portable. They allow no `${REPO_ROOT}/scripts/` refs, path escapes, or runtime `Skill()` calls between drivers. Build-time reuse uses `skill-bundles.yaml` and `fno doctor bundle`. The `bundle check` action gates freshness.
- **TDD:** failing test -> red -> minimal code -> green -> verify -> atomic commit.
- **Testing:** `python skills/execute/orchestrator.py --help`; `./scripts/validate-test-first.sh`.

## Plugin installation

```bash
claude --plugin-dir /path/to/footnote          # development
ln -s /path/to/footnote ~/.claude/plugins/fno  # permanent
```

## Deep-dive docs

Everything the body already links is reachable from the paragraph that explains it. These are the docs nothing above points at:

Backlog: [usage](docs/backlog-usage.md) · [board ordering](docs/architecture/backlog-board-ordering.md) · [triage](docs/backlog-triage.md)
Loop & target: [unified loop](docs/architecture/unified-loop.md) · [control-plane loop](docs/architecture/control-plane-loop.md) · [target reliability](docs/architecture/target-reliability-core.md)
Planning & ship: [lean blueprint](docs/architecture/lean-blueprint.md) · [plan completion stamp](docs/architecture/plan-completion-stamp.md) · [post-merge ritual](docs/architecture/auto-post-merge-ritual.md)
Coordination & providers: [coordination](docs/architecture/coordination.md) · [mux selector resolution](docs/architecture/mux-selector-resolution.md) · [provider rotation](docs/provider-rotation.md) · [harness command matrix](docs/harness-command-matrix.md) · [cross-model review](docs/architecture/cross-model-review.md)
Platform & ops: [harnesses](docs/HARNESSES.md) · [multi-CLI hooks](docs/architecture/multi-cli-hooks.md) · [path config](docs/path-config.md) · [disposable deletes](docs/architecture/disposable-deletes.md)

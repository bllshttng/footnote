# Frontend-craft gate

Stops frontend work from silently skipping the frontend-craft (`/impeccable`)
pass. Before this change, three independent gaps combined so a `/target` run
could ship a new UI surface as plain code with no craft pass and no warning:

1. `has_ui` defaulted to `false` at init (M profile), even for obvious UI
   changes. The changeset does not exist at init time, so the planner could
   not pre-declare it correctly. This silently skipped the browser gate too.
2. Executor resolution (`impeccable` for frontend files) only ran on the
   `/do waves` dispatch path. The inline `/do` path and the
   substituted-executor path bypassed it, so frontend work ran as plain `do`.
3. Nothing enforced that a craft pass ran. Skipping it was invisible.

The fix splits into two correctness fixes plus one config-gated enforcement
gate, layered so each builds on the previous.

## Bug 1: gate-time `has_ui` derivation

`scripts/lib/gate-audit.sh` re-derives `has_ui` from the session's git
changeset at promise time (when the files actually exist), using the locked
surface globs. Only a `false` value is upgraded; an explicit `has_ui: true`
is left alone. The derivation is a safe no-op when `REPO_ROOT` is not a git
repo. This restores the browser gate for UI changesets and feeds the
frontend-craft gate its "frontend touched" signal.

The single source of truth for the globs is the in-package module
`fno.executor._surface` (ported from the retired `infer-task-executor.sh`):
`is_frontend_surface_path()` / `any_frontend_surface()` plus a
CLI (`python3 -m fno.executor._surface`, with a `--has-ui` mode).
`infer-has-ui.sh` delegates to that module so routing and `has_ui` share one
copy of the patterns.

## Bug 2: executor resolution on the inline path

`scripts/lib/resolve-plan-executor.sh` resolves the executor for a flat plan
(the `/do` granularity), extracting the plan's declared file list plus any
plan-level `executor:` and running the same three-tier resolver `/do waves`
uses per task. `/do`'s skill body (step 1b) consults it so a frontend plan
routes to `/impeccable` on the inline path too, making the gate satisfiable
rather than a deadlock.

## Bug 3: the config-gated frontend-craft gate

`scripts/lib/frontend-craft-gate.sh` provides a pure decision function plus
config/presence resolvers; `gate-audit.sh` calls them at promise time. The
gate is **path-independent**: it does not care whether frontend work ran via
`/do waves`, inline `/do`, or substitution, only whether frontend surfaces
were touched and the craft pass ran.

```
config.gates.frontend_craft: off | warn | block   # default: warn
config.gates.frontend_craft_executor: <name> | none  # optional presence override
```

| Condition | Result |
|-----------|--------|
| mode `off` | disabled (no-op) |
| no frontend-craft executor installed | no-op |
| not a UI change (`has_ui` effective false) | not applicable |
| UI change, craft pass ran | gate satisfied |
| UI change, craft pass missing, mode `warn` (default) | hook-event + notify, does NOT block |
| UI change, craft pass missing, mode `block` | promise blocked |

The default is `warn`, OSS-safe: a brand-new install without `impeccable`
gets a no-op, and an install with `impeccable` gets a nudge rather than a
hard stop. Set `config.gates.frontend_craft: block` to make it bite.

### Satisfying the gate

The craft owner (`frontend-executor` via `/do waves`, or `/impeccable` via
`/do`) flips the gate and writes the artifact:

```bash
fno gate set frontend_craft_passed true
# + write .fno/artifacts/frontend_craft-<session_id>.md
#   frontmatter: phase: frontend_craft, session_id, approved: true
```

See [skills/target/references/gate-artifacts.md](../../skills/target/references/gate-artifacts.md).

### Presence detection

`frontend_craft_present()` resolves in order: the `FRONTEND_CRAFT_PRESENT`
env seam (tests / explicit operator override), then
`config.gates.frontend_craft_executor`, then a filesystem probe for an
installed `impeccable` skill. It defaults to ABSENT when nothing is found,
so the gate stays a no-op for installs that never added a craft executor.

## Follow-up

The v1 gate is two-factor (state boolean + session-scoped artifact). The
provenance third factor (a nonce-bound `phase_transition` event, matching
the other gates' forgery resistance) is deferred. `fno gate set` already
emits that event, so the follow-up is adding `frontend_craft` to the
`PROVENANCE_CHECKS` loop behind the gate's mode-aware disposition (so a
`warn`-mode miss never hard-blocks via the generic provenance loop).

## Tests

- `tests/lib/test_frontend_surface.sh` - dual-mode locked matcher
- `tests/lib/test_infer_has_ui.sh` - has_ui from a changeset
- `tests/lib/test_resolve_plan_executor.sh` - inline-path resolution
- `tests/lib/test_frontend_craft_gate.sh` - decision matrix, config, presence, changeset detection

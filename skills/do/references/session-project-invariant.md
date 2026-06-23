# Session-Project Invariant (G2)

**A `/do` session operates only in its own project.** When a wave (or a flat
plan's change) belongs to a *different* project than the session, `/do` must
NEVER `cd` into that repo and edit it. Instead it spawns a fresh `/target`
worker into the foreign project (when the foreign node is unblocked) or defers
to the merge-triggered dispatch (when it is still blocked). This makes the
"backend → frontend handoff" an emergent property of the dependency graph
rather than a special cross-project mode.

This is the new model. The legacy `scope: cross-project` pipeline still exists
(retired in a later PR) and is reached only via target's HARD GATE; it never
enters `/do`. The invariant below applies to ordinary single-session plans that
happen to carry a foreign wave.

## Resolving the session project

The session project is the executing plan's project:

- folder plan: `project:` in `00-INDEX.md` frontmatter
- single-doc plan: `project:` in the doc frontmatter
- fallback: the `project` of the backlog node in `.fno/target-state.md`'s
  `plan_path` (resolve with `fno backlog get <node> --field project`)

## Per-wave decision

For each wave, read its `project:` field (waves carry one in the Execution
Strategy). A wave with no `project:` or whose `project:` equals the session
project is **local** — execute it normally. Otherwise it is **foreign**, and
the wave's `node:` field names the foreign backlog node to dispatch.

```
WAVE_PROJECT == "" or == SESSION_PROJECT   ->  execute locally (unchanged)

foreign wave:
  no `node:` on the wave                    ->  REFUSE (authoring error)
  fno backlog project-root <WAVE_PROJECT>   ->  exit 1 (unmapped)  ->  REFUSE by name
  fno backlog get <node> --field _status:
      ready  (unblocked, has a plan)        ->  SPAWN into <root>
      blocked                               ->  DEFER (carveout) + rely on G1
      idea   (unblocked but plan-less)      ->  skip; needs /blueprint first, not
                                                auto-dispatched (matches G1, which
                                                dispatches ready-only)
      done                                  ->  skip (already shipped)
      claimed                               ->  skip (a worker already owns it)
```

Every branch prints a one-line receipt or deferral. **No foreign wave is ever
silently skipped.**

### Commands

```bash
# 1. Resolve the foreign project's root (null/exit-1 == unmapped -> refuse).
ROOT=$(fno backlog project-root "$WAVE_PROJECT") || {
  echo "do: REFUSE foreign wave - project '$WAVE_PROJECT' is not in config.work.workspaces; refusing to guess a cwd" >&2
  # mark the wave refused in STATE.md; do NOT spawn, do NOT cd, continue own waves
}

# 2. A foreign wave with no node: is a /blueprint authoring error.
if [[ -z "$WAVE_NODE" ]]; then
  echo "do: REFUSE foreign wave for '$WAVE_PROJECT' - no node: reference (every cross-repo shippable unit must be a backlog node)" >&2
fi

# 3. Branch on the foreign node's status.
STATUS=$(fno backlog get "$WAVE_NODE" --field _status)
case "$STATUS" in
  ready)
    # SPAWN into the foreign project. --cwd (never -P); subscription lane (never -p/--bare).
    fno agents spawn --provider claude --cwd "$ROOT" "target-$WAVE_NODE" "/target $WAVE_NODE"
    echo "do: spawned target-$WAVE_NODE --cwd $ROOT"   # receipt (AC3-UI)
    # mark the wave DELEGATED in STATE.md; continue the session's own waves
    ;;
  idea)
    # Unblocked but plan-less: not "ready" work. Skip (a human /blueprint authors
    # it first), matching G1's ready-only dispatch so the two pillars agree.
    echo "do: skipped $WAVE_NODE ($WAVE_PROJECT) - no plan yet; /blueprint it first"
    ;;
  blocked)
    # DEFER: a spawned worker would refuse on the still-blocked node. Record it
    # and rely on G1 (advance_dependents dispatches it when the blocker merges).
    fno carveout add --kind deferred \
      --need "blocked foreign node $WAVE_NODE (project $WAVE_PROJECT)" \
      "foreign wave $WAVE_NODE blocked at /do time; G1 dispatches on blocker merge"
    echo "do: deferred $WAVE_NODE to $WAVE_PROJECT; dispatch on blocker merge"   # AC3-UI
    ;;
  done)     echo "do: skipped $WAVE_NODE ($WAVE_PROJECT) - already shipped" ;;
  claimed)  echo "do: skipped $WAVE_NODE ($WAVE_PROJECT) - a worker already owns it" ;;
  *)        echo "do: skipped $WAVE_NODE ($WAVE_PROJECT) - status '$STATUS'" >&2 ;;
esac
```

The agent name `target-$WAVE_NODE` matches the dispatch-node / advance worker
naming so the same-name spawn-collision dedup keeps a successor single-worker.

## Hard rules

- **Never** `cd` into the foreign repo and edit it. The whole point of G2 is to
  replace `cd $other && work-on-main` with a spawn into that project.
- A spawned worker resolves its own project from `<node>` and runs independently
  in `<root>`. The session edits zero foreign files and continues its own waves.
- A refused or deferred foreign wave does NOT fail the session — record it and
  carry on. The session's own (local) waves still ship a PR.
- The merge-triggered path (G1 `advance_dependents`) is the backstop for every
  deferred foreign node, so nothing is dropped: when the blocker's PR merges,
  the dependent is dispatched into its own project automatically.

## Flat mode

A flat plan is a focused single-file plan with no waves, so it is single-project
by construction. The invariant collapses to one rule: if a change would edit a
file outside the session project's repo root, STOP — do not `cd` and edit it.
Surface the foreign work as a backlog node and spawn `/target <node>` into that
project (the `ready|idea` branch above), or, if there is no node yet, report it
so the user can `/blueprint` + decompose it. Flat mode never edits a second repo
in place.

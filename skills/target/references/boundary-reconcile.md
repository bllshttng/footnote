# Boundary reconcile (Step 0)

**Only runs when the orienter printed `boundary-reconcile: STALE`.** A fresh plan
(`fresh`), an already-reconciled one (`reconciled`), or a failed detection
(`unknown`) all skip this step silently - Step 0 is a no-op except on STALE.

## What this is (and is NOT)

When `/target` picks up a node whose plan/brief was written **before** a blocker's
PR merged, the fresh-context worker would build on stale assumptions. The mux 4a
epic proved this bites every time: a manual reconcile pass at each phase boundary
caught real drift on every run (alacritty drops unknown OSC; the `Session` model
lives in `squad.rs` not `tree.rs`; a proto-bump hedge resolved by G1 shipping
separately). Step 0 makes that pass mechanical to detect and mandatory to perform,
builder-side - no defer/undefer parking brake, no reconcile pre-worker.

**Distinct from `--reconcile <manifest>` (de-stub reconcile).** That mode
(`references/reconcile-mode.md`) serves a `contract` dependent that stubbed against
an unlanded interface and reuses a draft PR. **Boundary reconcile** serves a
hard-serialized dependent that never stubbed anything: it just needs to read what
its blocker landed before writing code. The two never touch each other's state.

## Scope

- **`/target` only.** A bare `/do <plan-path>` or `/fix` has no node context to
  compute blockers from - out of scope. `/megawalk` dispatches via `/target`, so
  it inherits Step 0 for free.
- **No enforcement.** The orienter line is advisory; this step is a spine
  obligation under the same trust model as every other spine step. There is no
  hook, no Rust gate, no `target-state.md` field.

## Procedure

The orienter line names each stale blocker and its PR:

```
boundary-reconcile: STALE vs x-e317 (PR #141, merged 2026-07-02) - Step 0 required
```

For **each stale blocker independently**, before the run's first code commit:

1. **Read its merged diff.** `gh pr diff <n>`. Fallback if `gh` is unreachable:
   `git log origin/main --grep "(#<n>)" --format=%H | head -1` (the newest
   squash-merge commit whose subject carries the PR number), then `git show <sha>`.
2. **Append the blocker's boundary-reconcile section** (format below) to the plan
   or brief - the same file the orienter checked (`plan_path`, or the node's
   brief). **Append a SECTION only; never rewrite a shared plan's frontmatter**
   (a shared `#group` doc's frontmatter belongs to its owner - see
   `reference_decompose_child_clobbers_shared_doc`).

All stale blockers get their sections before task 1 starts. A diff failure
degrades only that one blocker's section (below), never the whole pass.

## Section format (standardize on this)

Taken from the phase4a exemplar
(`internal/fno/plans/2026-07-02-fno-mux-phase4a-agent-edge.md`):

```markdown
### <blocker-id> landed (PR #<n>, merged <YYYY-MM-DD>) - boundary reconcile

<one line: did the landed diff shift this plan's design? UNSHIFTED, or what moved.>
Landed facts this plan consumes - do not re-derive:

1. <concrete fact: struct shape, proto version, exit code, resolved open question>
2. ...
Task adjustments: <none | the task-note edits made in this pass>
```

The heading's **`<blocker-id> landed`** token is the machine-greppable idempotence
marker; the PR number in parens is the secondary match key. On a resume/handoff/
redispatch the orienter finds this marker and reports `reconciled`, so the section
is written **once** per (plan doc, blocker) pair even across sessions.

## Degraded path (per blocker)

If, for a given stale blocker, **both** `gh pr diff` and the local squash-commit
fallback fail (offline, auth), do NOT silently skip it:

1. Append the section anyway with a `(DEGRADED: diff unreachable - <reason>)`
   suffix in the heading body, listing what could not be verified.
2. Record the gap: `fno carveout add --kind deferred --need "verify <blocker> landed shapes" "boundary reconcile for <blocker> ran degraded - diff unreachable"`.

The marker still lands (idempotence holds); the verification gap is surfaced at
merge-time retro-triage instead of swallowed.

## Why builder-side

The fresh-context worker must read the landed code anyway. Doing the reconcile
itself means no new daemon surface, no reconcile pre-worker whose entire output
the builder re-reads, and no new state file - the appended section IS the durable
state. This deletes the manual defer/undefer parking brake as a sequencing tool:
`fno backlog advance` and lane-fill can dispatch a just-unblocked dependent
immediately, because the dispatched worker reconciles itself.

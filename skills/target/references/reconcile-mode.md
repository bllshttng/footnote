# Reconcile mode (`/target --reconcile <manifest> <node>`)

The G4 de-stub pass. A `contract` dependent built optimistically against a
pinned interface (G3): it stubbed the unlanded parts, opened a **draft** PR, and
wrote `.fno/stub-manifest-<node>.json`, then ended. When the blocker's PR merges,
`fno backlog advance` (via `backlog.reconcile_dispatch`) spawns a fresh
`/target no-merge --reconcile <manifest-path> <node>` worker. This reference is
that worker's contract.

A reconcile run is a **constrained** `/target`: it reuses the dependent's
EXISTING draft PR rather than creating a new one, and its "do" work is swapping
stubs for the real implementation now that the blocker has landed.

## Detection

The command carries the `--reconcile <manifest-path>` token (alongside
`no-merge`). When you see it in ARGUMENTS, you are in reconcile mode. Resolve:

- `RECONCILE_MANIFEST` = the path after `--reconcile`.
- `NODE` = the trailing node id.
- `ROOT` = the dependent's project root (cwd of this session).

## Pipeline

Init as normal (`fno target init`), then before the do phase:

### 1. Pull main (the blocker's schema is now landed)

```bash
git fetch origin && git rebase origin/main   # or merge, per project convention
```

### 2. The drift gate (Locked Decision 5 - executable truth, never a doc diff)

```bash
fno stub-manifest reconcile-validate --node "$NODE" --root "$ROOT"
```

Branch on the exit code:

| Exit | Outcome | Action |
|------|---------|--------|
| 0 | `authorize` (suite passed) or `already-reconciled` | proceed to de-stub (or, if already reconciled, just confirm the PR is ready) |
| 3 | `drift` (contract-test failed OR no suite in manifest) | **REFUSE** auto-de-stub (step 4) |
| 4 | `manifest-missing` (missing/partial manifest) | **REFUSE** + surface the gap (step 4) |

The gate fails CLOSED: a missing contract-test suite is treated exactly like a
failing one. Never guess the landed schema is fine.

### 3. De-stub (only on `authorize`)

Read the manifest's `stubs[]`. For each stub, replace the mocked
file/symbol with the real implementation now that the blocker's schema is on
main. **This is a real implementation pass, not a find-replace** (Domain
Pitfall): de-stubbing surfaces integration bugs the stub hid. Run the full test
suite, not just a swap.

Then finalize and flip the PR ready:

```bash
fno stub-manifest reconcile-finalize --node "$NODE" --root "$ROOT"   # clears the hold signal
gh pr ready <pr-number>                                              # draft -> ready
```

`reconcile-finalize` flips the manifest's `reconciled` flag so `fno pr merge`
stops refusing the PR. The `gh pr ready` flip is the skill's job (kept out of the
pure CLI state write). Emit `<promise>` only after CI is green on the now-ready PR.

### 4. Drift / missing-manifest refusal (AC4-ERR / AC5-FR)

Do **not** de-stub. Do **not** flip the PR ready (it stays draft, so it can never
merge with mocks). Surface the gap so a human resolves it:

```bash
fno carveout add --kind oos-bug --priority p1 \
  "reconcile drift on $NODE: landed schema failed the contract-test gate; auto-de-stub refused"
gh pr comment <pr-number> --body "⚠️ Reconciliation refused: <validate detail>. The landed schema failed the contract-test gate (or no executable gate exists). De-stubbing needs a human - this PR stays draft until then."
```

Then end the session WITHOUT a completion promise - the work is genuinely
blocked on human judgment. The stranded draft PR is surfaced by
`fno backlog triage health` (x-a10e stranded-dependent reuse).

## Invariants

- A reconcile run NEVER merges (it rides `no-merge`); it only flips a draft to
  ready after the gate authorizes + tests pass.
- A reconcile run NEVER creates a new PR; it finalizes the dependent's existing
  draft PR.
- On any refusal the PR stays draft - mocks never ship (Locked Decision 4).

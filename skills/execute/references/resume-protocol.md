<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# Resume Execution Protocol

Two resume layers compose: the lightweight STATE.md wave/task trail (below) and
the durable typed receipt (x-c3a2, the authority-safe layer). A successor reads
both; it WRITES only after the durable receipt revalidates against live state.

## Durable typed resume receipts (x-c3a2)

A receipt is durable **evidence** of where a session left off, never write
**authority**. The authority to resume is the live claim + liveness + git HEAD,
revalidated against the receipt before any successor write.

- Schema + writer + validator: `cli/src/fno/resume/receipt.py`.
- CLI: `fno do resume receipt write|validate|show`.
- Producer call site: `skills/target/scripts/handoff.sh` writes one at every
  handoff boundary, alongside the human-readable brief.
- No resume database. Canonicalization reuses the scoreboard reducers
  (`fno.scoreboard.fold`): dedup by signature, latest-by-parsed-timestamp with a
  deterministic complete-on-tie. Duplicated/out-of-order events across the
  global + delivery-root journals fold to one observation.

### Producer behavior

Written at a phase/handoff boundary. Identity is
`(node, session, phase, generation, candidate_sha)` where `candidate_sha` is the
short git HEAD. One immutable file per identity under
`.fno/artifacts/handoff/receipt-<node>-<phase>-g<gen>-<sha>.json`; `write`
refuses to overwrite, so a new HEAD at the same phase/generation is a new
version, not a clobber. Exactly one `next_action`. A `content_sha` integrity tag
detects tamper on load.

The producer is best-effort at the handoff site: a receipt write failure does
NOT abort succession (the brief + `delegated` event remain the primary path).

### Consumer behavior (revalidate before any write)

A successor session runs `fno do resume receipt validate --node <id>` before its
first write. The validator gathers live HEAD/branch, worktree existence, the
node-claim holder, and the node's journal events, then runs the read-only
`revalidate` gate. It **never acquires or releases claims** - on failure the
caller parks and the predecessor's state and claims are preserved by
construction.

Continuation is allowed only when `revalidate` returns ok AND the successor
acquires `node:<id>` through the canonical claim primitive (`fno agents claim` /
`acquire_claim`). A free claim is ok (the successor acquires); a claim held by
the receipt's own session is ok (idempotent re-acquire).

Fail-closed cases (each returns a machine-readable `reason`, exit 1):

| reason | trigger |
|---|---|
| `stale_head` | receipt HEAD != live HEAD (the candidate moved) |
| `stale_branch` | receipt branch != live branch |
| `dead_worktree` | recorded worktree path no longer exists |
| `foreign_claim` | node held by a session outside the receipt's lineage |
| `duplicate_generation` | a `delegated` event already minted this generation off-session |
| `superseded_by_later_event` | a later terminal/delegated/loop_check for the node postdates the receipt |
| `malformed_receipt` | present-but-corrupt receipt file (never read as empty evidence) |

### Idempotency keys (external-effect replay prevention)

A receipt carries `idempotency_keys` for non-idempotent external effects
(`pr_create:<sha>`, `comment:<sha>`, `merge:<sha>`, `publish:<sha>`). A worker
that died after the effect but before journaling is detectable: a resumed worker
checks the latest receipt's keys before performing the effect and skips any
already recorded, so a publish, PR create, comment, or merge cannot replay.

### Cross-harness wire shape (PR #611 lesson)

Resume re-injection after compaction differs by harness and must not be assumed
to one shape: Codex `PostCompact` accepts `systemMessage`; Claude uses
`hookSpecificOutput.additionalContext`. Any cross-harness resume carrier reads
both shapes and never hardcodes one. `fno do resume receipt validate` is
harness-agnostic (it reads git + claim + journal, not harness-specific carrier
fields), so a successor on either harness revalidates identically.

## STATE.md wave/task trail (lightweight)

If execution was interrupted:

```bash
cat .fno/STATE.md 2>/dev/null || echo "No state found"
```

From STATE.md, extract completed waves (lines with `[x]`), completed tasks
within each wave, and the last wave in progress. Skip completed waves; for a
partially complete wave, continue from the next task (sequential) or re-run only
failed tasks (parallel). `/execute waves --resume` reads STATE.md and continues from
the next incomplete wave/task.

This trail is per-session context; the durable receipt above is the
authority-safe resume layer that survives process death.

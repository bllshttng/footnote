# Target capability escalation

## Problem

Target succession used to run automatically at blueprint/do and wave boundaries. The wave path interpreted context percentage as a reason to create a new session, while the universal context nudge prescribed compaction for the same condition. The two paths disagreed about identity: compaction preserves the session id, mail handle, claim, worktree, branch, and PR, while succession must transfer or reconstruct each one.

The automatic path also collapsed five independent axes. It named the child from the parent harness, launched a hardcoded Claude process, omitted the selected model and account, treated registry liveness as readiness, released the parent claim before proving the child could work, and emitted `delegated` before proving the target command executed.

## Trigger contract

Context pressure triggers compaction. It never triggers capability escalation. Blueprint/do and wave boundaries continue in the current session.

Capability escalation is explicit. An external operator or supervising king selects a stronger destination after reading evidence that the current worker cannot finish. The worker may signal that it is stuck; it may not select its own successor.

Invoke the bundled transaction with an explicit destination:

```bash
bash skills/target/scripts/handoff.sh \
  --harness claude \
  --model claude-opus-5 \
  --account makers
```

Use `--dispatch-account <provider-record>` instead of `--account` for an autonomous destination record. The two flags are mutually exclusive. Raw `--settings` is not a public carrier; the spawn layer resolves account and route identity, then generates the correct settings file internally.

## Two-proof transaction

The helper owns the state-changing sequence. Its first proof happens while the parent still owns everything. Its second proof happens after the target seed is submitted.

| Step | Action |
|------|--------|
| 1 | Verify the parent manifest, ready plan, live node claim, feature switch, and one unused escalation rung. |
| 2 | Resolve the node slug and mint `target-<full-node-id>-<slug>-g2`. |
| 3 | Build a random challenge whose expected SHA-256 digest covers the nonce, exact working directory, and exact git root. The expected digest never appears in the prompt. |
| 4 | Spawn the selected harness/model/account on a pane with the read-only challenge. Require a clean receipt whose name, harness, model, account, binding, and positive readiness marker match the request. |
| 5 | Poll `fno agents truth` for the exact `FNO_CAPABILITY_READY:<digest>` assistant response and `observed_model.kind=observed` with the configured model. Registration, process liveness, prompt readiness, silence, a logged-out pane, or another model cannot satisfy this proof. |
| 6 | Write the handoff brief and durable resume receipt, acquire `dispatch:<node>`, archive the parent manifest, and release the parent node claim. |
| 7 | Raw-submit the harness-native target command, including `--no-merge` when the parent manifest refused merge authority. A delivery receipt proves delivery only. |
| 8 | Require `node:<id>` to be held by `target-session:<child-session>` and require the new target manifest to carry the same child harness session plus the same node id. Only this claim-plus-manifest pair proves the target seed executed. |
| 9 | Emit `delegated` with `handoff_kind=capability_escalation` and the actual destination harness/model/account, then finalize the parent ledger row. |

## Failure and unwind

A failure before Step 6 stops and removes the uncommitted child while leaving the parent claim and manifest untouched. It emits `handoff_failed{reason=capability_probe}` and returns `parked`.

A target-seed failure or missing child claim/manifest proof stops the child, releases any child claim held by the proven child identity, reacquires the parent claim, restores the archived manifest, releases the dispatch bridge, and emits the exact failed stage.

If parent reacquisition fails, the manifest remains archived and the helper exits 12 with `handoff-claim-lost`; the parent must not continue. If manifest restoration fails after reacquisition, the helper exits 12 with `handoff-restore-failed`.

| Exit | Meaning |
|------|---------|
| `0` | Capability and target-execution proofs passed; delegation committed. |
| `2` | Usage error, including missing harness/model or retired boundary flags. |
| `10` | Refused or failed with parent ownership untouched or restored. |
| `12` | Parent ownership could not be restored; stop immediately. |

## One-rung ladder

The external caller selects the strongest destination directly. A `delegated` event with `handoff_kind=capability_escalation` spends the node's one escalation rung. A second attempt refuses as chain exhausted. Historical boundary-delegation events do not consume this rung.

The former `target.handoff.generation_cap` setting is retired. Legacy config containing it remains loadable because the config block ignores unknown fields, but the value creates no additional rung.

## Context compaction

`hooks/context-nudge.sh` remains the single context-pressure decision path. `target.handoff.used_pct_trigger` and `target.handoff.king_used_pct_trigger` now describe general and king compact nudges. The retired PreCompact arm hook and PostCompact handoff marker no longer exist. Normal PostCompact target context reinjection remains.

## Observability

New `delegated` rows carry the bound child session, destination harness, configured model, optional account, and `handoff_kind=capability_escalation`. Legacy rows remain readable for lineage audits.

`handoff_failed.reason` names the failed stage: `capability_probe`, `target_seed`, `target_execution`, `release_failed`, `reacquire_failed`, or `restore_failed`. A registry row or send receipt is never promoted into a success event.

The child name, spawn receipt, transcript-observed model, claim holder, target manifest, and delegated event must agree. Any disagreement fails closed before `delegated` is written.

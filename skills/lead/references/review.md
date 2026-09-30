# Reviewing a worker's change

Workers review completed diffs inline in the same session and worktree that built them. This keeps review attached to the builder's actual tree and lets the worker address findings without a king-mediated handoff.

| Harness | Invocation |
|---|---|
| Codex | `$fno:review <level> --comment` |
| Claude | `/fno:review <level> --comment` |

Choose `medium` below 300 changed lines, `high` at 300 or more, or `xhigh` for risky state or protocol changes. Verify each finding against source and fix the valid ones. If fixes change the tree, run round two with `--verify-fixes` so the final head receives its own review result.

Never mail the king to fire a native review command. Never spawn a review subagent or substitute a shell approximation. If the inline fno review skill refuses, report its exact refusal and stop. Do not end the worker turn on an unconfirmed paste that needs someone else to resume it.

## Repeated findings in the peer lane

The `peer` lane requires a machine verdict with zero blocking findings. `consume-peer-verdict.sh` emits a pass only for that verdict. A repeated blocking finding keeps its gate closed, even after the author responds.

This is usually correct. The hard case is a reviewer who asks for a fact that the current code cannot compute. A config-only renderer cannot identify which peer uses the author's model. It lacks the active session's harness.

Five peer passes on one PR each found a real issue. One claim survived because it asked the code to state a fact it cannot compute. That claim stopped describing a defect in the diff. It described a design limit.

Count repeated claims, not rounds. On a third repeat of the same accurate limitation, rule on the limit instead of ordering another pass.

- **Authorize the dependency in a separate PR.** Keep it out of the PR under review.
- **Rule the expectation unsatisfiable from that file.** Ship with the peer lane unmet and state why in the PR body.

A stated unmet gate is honest. A claim nobody can verify cannot clear it.

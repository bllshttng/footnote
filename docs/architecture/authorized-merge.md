# One authorized merge operation

`crates/fno-agents/src/authorized_merge.rs` answers one question for every path that lands a PR. Is this exact head authorized to merge or to arm, and what happens next.

## Why it exists

Two callers used to answer that on their own. They answered it differently.

`fno do pr merge` ran a long guard chain. The plan hold, the in-flight review hold, the posture fold, the automerge floor, the coverage gate, the checks verdict, the base lineage. Then it merged.

`fno-agents finalize` armed GitHub's native auto-merge queue at a green terminal. Its chain was shorter and different. It never read the in-flight review hold, so a queue armed at the terminal shipped the code a review was still fixing. When it cannot read the covered head, it also dropped `--match-head-commit`. A racing push then landed an unreviewed head through the queue.

A guard on one of two reachable merge paths is decorative. The two chains are one chain now.

## The shape

A caller builds a `Request` and reads one `Outcome` back.

```rust
let outcome = authorized_merge::run(
    &authorized_merge::RealProbes,
    &Request { cwd, pr, effect: Effect::Arm, approved, auto_merge_source, .. },
);
```

`Effect::Merge` merges now. `Effect::Arm` hands the PR to GitHub's queue. The decision is identical. Only the effect differs.

The receipt keeps them apart. `Armed` is a queue entry, a promise GitHub keeps later. `Merged` is a merge that landed. A caller that reads one as the other stops watching a PR that never merged.

| Outcome | Means | Caller does |
|---|---|---|
| `Merged` | the merge landed. `note` says how. `cleanup_failure` names a post-merge step that failed | post-merge follow-ups |
| `Armed` | the queue owns it now | nothing; the queue merges on green |
| `Authorized` | a `decide_only` pass cleared; nothing ran | its own pre-effect step, then ask again |
| `Held` | retryable: a hold, a pending check, an already-armed PR | retry later |
| `Refused` | needs an operator: no grant, below the floor, a stale base, an unbound PR | escalate |
| `HeadChanged` | the head moved between validation and the effect | re-evaluate |
| `Unknown` | an instrument could not answer | retry; never read it as clear |
| `Failed` | the effect ran and failed | report |

The gate merges a PR only after its CI tested the current main. The gate reads the PR's `pull_request` runs at its head. If a PR is behind main and has no run, or its oldest current run predates main's tip, it holds as `ci_base_stale`. No file-overlap rule waives this. Two PRs that share no file can still break main together: a test in one reads what the other moved. An unreadable freshness read answers `Unknown`, never a merge. The gate covers `Effect::Arm` too. Main has no branch protection, so GitHub merges an armed green PR at once.

After a main merge, every open PR with older CI is stale. While the slot is free, `decide` holds the first stale PR with `ci_base_stale` and gives it the slot. The holder then has GitHub merge main into its branch (`PUT pulls/<n>/update-branch`, pinned to the head it read). That is a merge commit, never a rebase, so CI reruns against the current main. The review coverage carries over as `carried_base_sync`. A conflict or a failed call holds with the manual remedy: merge `origin/main` into the branch and push. The receipt says `merge_slot_held`. While a PR holds the slot, all others are `Held`, even with fresh CI. This keeps another merge from invalidating the holder's retest. When the holder merges or closes, turns red, takes a dispatch hold, or its 60-minute lease expires, the slot releases. A held holder can satisfy none of the other three, so a hold releases the slot. An admitted holder that arms keeps the slot until its merge lands. The 60-minute TTL covers one retest, the measured 31-minute rust-ci maximum, one 10-minute sweep tick, and margin. The lease uses `<canonical repo>/.fno/claims`, keyed `merge-slot:<base_ref>`.

## The decision, in order

1. **One guarded fetch.** `fno do pr info` gives the number, the head, the state, the body, and whether GitHub's queue owns the PR. The armed flag rides that same payload. A second `gh pr view` probe describes a different head.
2. **Terminal state.** A merged or closed PR holds before every other guard. The guards below protect a merge that has not happened yet. An "unreviewed" answer about a landed merge sends a caller hunting a defect that blocks nothing.
3. **Authority.** A per-run refusal (`auto_merge_approved: false`) outranks every grant, except the head-scoped operator grant below. An explicit per-run env grant (`auto_merge_source: env-target-auto-merge`) satisfies the standing arm on its own. Otherwise the LIVE config decides. A manifest snapshot never outlives an operator who flips the switch off mid-flight. Then the automerge posture floor.
4. **Node binding.** The graph must see the PR. Three keys decide, in order. A node id the branch names. A node whose own back-pointer carries the PR. A closure line in the body (`Fixes <id> [<id>...]`. The retired `Backlog-Closure:` spelling still reads). The same predicate answers the king board's untracked warning, so the board and the gate cannot disagree. When all three miss, the merge refuses. The remedy is to bind it: pick or file the node, write the closure trailer onto the body, retry. A revert or a hotfix binds the same way. There is no bypass flag. The team merges, and not only the operator. A bypass flag is one the agents pass to themselves. The gate stays silent where the repo keeps no backlog. A graph with no node under the canonical root has nothing to bind to. A graph or a body that cannot be read is `Unknown`, never a verdict.
5. **The dispatch hold** (`fno do pr hold-check`), fail-closed.
6. **The in-flight review hold** (`fno do pr review-hold check`), fail-closed. Coverage answers what verdicts EXIST for a head. It cannot say that a review runs right now with its findings uncommitted.
7. **The pin.** The covered head comes from the caller's own coverage gate, or from the `review_coverage` journal. An unreadable head is `Unknown`. There is no unpinned fallback. A head that no longer matches the PR's is `HeadChanged`.
8. **Base lineage** (`fno do pr base-lineage-check`), fail-open. A refusal on a gh hiccup makes auto-merge silently never work. That reads exactly like nobody opting in.
9. **Merge result** (`fno do pr merge-result-check`), fail-open. The merge tree is computed locally and the repo-wide ruff + mypy step runs on it. The same merge tree also runs the repo's preamble budget from its own copy. Two PRs that each pass their own base can no longer merge into a main over the byte ceiling. When git joins hunks that never met on one machine, two green parents merge red. Held, not refused: the remedy is rebase, fix, push, retry.
10. **Checks**. The caller asks for these or leaves them out. `--auto` IS the wait for the checks, so the arm path leaves them out and the queue enforces them server-side.

## The head-scoped operator merge grant

The per-run no-merge refusal used to name only out-of-band escapes: merge by hand, or re-dispatch. That door is closed. The one sanctioned remedy is a law row at a subject that names the exact head:

```
merge-grant:<owner/repo>#<pr>@<40-hex head>
```

An operator records it in their own terminal, attended:

```
fno backlog decide 'merge-grant:owner/repo#2131@abc...' 'merge authorized for this head' --authority operator
```

The reader lives in `merge_grant.rs` (`head_grant_status`) and shares the waiver's trust shape. Only rows whose `authority_source` is `operator` count. A harness session cannot mint one, because `fno backlog decide` refuses `--authority operator` from any agent session. The decision must equal `merge authorized for this head` exactly: row existence carries no polarity, identical duplicates read granted once, and disagreeing rows read conflicting. A `chat_attested` row at the subject is invisible to the reader.

Head invalidation is structural. A push changes the head, the subject no longer matches, and the new subject has no rows: the refusal stands until the operator grants again. No manifest is rewritten.

The grant supersedes only the per-run layer. Live config, the posture floor, the holds, the pin, and every later guard still run. A merge that passes them carries `"merge_grant": "operator head grant <short head>"` in its receipt.

## The effect

`gh pr merge <n> [--auto] --<strategy> --match-head-commit <head>`.

The pin is never optional. It is the last guard between the decision and a racing push. If the head moved, gh refuses the whole call.

No `--delete-branch`, on either path. Its LOCAL delete fails from inside the worktree that holds the branch. That made the best-disciplined merge the one most reliably reported failed. Remote cleanup is a separate step.

A non-zero gh exit is never taken at face value. The PR's own state is re-read. When a post-merge step fails after the server-side merge landed, gh also exits non-zero. A branch another worktree holds recovers through the REST endpoint, which carries the same `sha` pin.

The recovery and the cleanup failure ride separate fields. A recovery that worked is how the merge landed, not trouble around it. Folded together, every worktree-held merge reported partial.

## Who calls it

- **`fno do pr merge`** (`cli/src/fno/pr/_merge.py`) asks twice. The `decide_only` pass comes first, so a merge about to be refused never publishes its coverage status receipt. The second pass re-runs the whole chain and closes the window the first one opened.
- **`fno do pr verify --kind merged`** (`cli/src/fno/pr/_verify.py`) asks once, then re-reads the PR before it reports. This verb's name is its contract, so the receipt is not its last word.
- **`fno-agents finalize`** arms at a green terminal. What stays in finalize is what only the terminal knows: the merge-gating opt-out claim, and the optional-App review evidence.

Python reaches it through `fno.rust_binary.verb_call("authorized-merge", payload)`, the single door. An unreachable binary answers `unknown`, never a clear merge. A merge whose authorization nobody read is not an authorized merge.

That includes a binary too OLD to know the verb. A deployed `fno-agents` from before this landed answers `unknown`, so `fno do pr merge` holds at exit 2 rather than merging unauthorized. The remedy is `fno doctor update --rust`, and `fno doctor` reports the lag. Hold, never break, was the point of routing the door through one named refusal.

## After a queue-armed merge

When the queue lands a merge later, no fno process is in the loop. The remote ref used to stay behind forever, because the merge verb's cleanup step never runs on that path.

The PR watcher tick already detects the confirmed merge. It pays the step there, through the merge verb's own `_post_merge_remote_delete`. One cleanup implementation. No second watcher, no second store. Only the MERGED arm reaches it, so a pending or unreadable state never deletes anything. It is warn-only: cleanup never fails the merge it follows.

Local branches and worktrees keep their own lifecycle. See [worktree-mechanics](worktree-mechanics.md).

## Merge provenance

Every merge hop writes one `decision_span` row to the project journal beside the repo. The row carries `trace.actor_session` and `trace.actor_kind`, so "who merged PR N" has a durable answer that survives the session.

The merge owner (`authorized_merge::run`, via `crates/fno-agents/src/merge_provenance.rs`) writes `merge_landed` or `merge_armed`. The `path` attr names the lane: `pr_merge`, `pr_watch`, or `finalize`. The gh proxy's draft door writes `merge_requested` with `path: gh_proxy` for every delegated gh merge argv. A PostToolUse Bash hook (`hooks/merge-capture.sh`, through the `graph-get` stdin door) writes `merge_requested` with `path: hook`. It is the backstop for gh that skipped the proxy.

When reconcile closes a node whose PR merged with no fno record anywhere, it writes `merged_outside_fno` with `merged_at` and `merge_sha`. No record means the user in the GitHub UI or another machine. The Rust close also stamps `closed_by: {session, actor_kind}` on the node row, and the daemon close adds `path: reconcile`.

GitHub alone cannot tell a fleet merge from a user merge while both share one token. The query that answers the node's own record is `jq 'select(.type=="decision_span" and .data.pr==N)' <repo>/.fno/events.jsonl`. A GitHub App identity for fleet merges is the standing question that will make GitHub itself show the difference.

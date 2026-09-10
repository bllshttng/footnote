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
| `Refused` | needs an operator: no grant, below the floor, a stale base | escalate |
| `HeadChanged` | the head moved between validation and the effect | re-evaluate |
| `Unknown` | an instrument could not answer | retry; never read it as clear |
| `Failed` | the effect ran and failed | report |

## The decision, in order

1. **One guarded fetch.** `fno do pr info` gives the number, the head, the state, and whether GitHub's queue already owns the PR. The armed flag rides that same payload. A second `gh pr view` probe describes a different head.
2. **Terminal state.** A merged or closed PR holds before every other guard. The guards below protect a merge that has not happened yet. An "unreviewed" answer about a landed merge sends a caller hunting a defect that blocks nothing.
3. **Authority.** A per-run refusal (`auto_merge_approved: false`) outranks every grant. An explicit per-run env grant (`auto_merge_source: env-target-auto-merge`) satisfies the standing arm on its own. Otherwise the LIVE config decides. A manifest snapshot never outlives an operator who flips the switch off mid-flight. Then the automerge posture floor.
4. **The dispatch hold** (`fno do pr hold-check`), fail-closed.
5. **The in-flight review hold** (`fno do pr review-hold check`), fail-closed. Coverage answers what verdicts EXIST for a head. It cannot say that a review runs right now with its findings uncommitted.
6. **The pin.** The covered head comes from the caller's own coverage gate, or from the `review_coverage` journal. An unreadable head is `Unknown`. There is no unpinned fallback. A head that no longer matches the PR's is `HeadChanged`.
7. **Base lineage** (`fno do pr base-lineage-check`), fail-open. A refusal on a gh hiccup makes auto-merge silently never work. That reads exactly like nobody opting in.
8. **Merge result** (`fno do pr merge-result-check`), fail-open. The merge tree is computed locally and the repo-wide ruff + mypy step runs on it. When git joins hunks that never met on one machine, two green parents merge red. Held, not refused: the remedy is rebase, fix, push, retry.
9. **Checks**. The caller asks for these or leaves them out. `--auto` IS the wait for the checks, so the arm path leaves them out and the queue enforces them server-side.

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

# One authorized merge operation

`crates/fno-agents/src/authorized_merge.rs` answers one question for every path that can land a PR: may this exact head be merged or armed under the current authorization, and what happens if it may.

## Why it exists

Two callers used to answer that on their own, and they answered it differently.

`fno do pr merge` ran a long guard chain: the plan hold, the in-flight review hold, the posture fold, the automerge floor, the coverage gate, the checks verdict, the base lineage. Then it merged.

`fno-agents finalize` armed GitHub's native auto-merge queue at a green terminal, after a shorter and different chain. It never read the in-flight review hold, so a queue armed at the terminal could ship the code a review was still fixing. And it dropped `--match-head-commit` when the covered head was unreadable, so a racing push could land an unreviewed head through the queue.

A guard on one of two reachable merge paths is decorative. The two chains are one chain now.

## The shape

A caller builds a `Request` and reads one `Outcome` back.

```rust
let outcome = authorized_merge::run(
    &authorized_merge::RealProbes,
    &Request { cwd, pr, effect: Effect::Arm, approved, auto_merge_source, .. },
);
```

`Effect::Merge` merges now. `Effect::Arm` hands the PR to GitHub's queue. The decision is identical; only the effect differs.

The receipt keeps them apart. `Armed` is a queue entry, a promise GitHub may keep later. `Merged` is a merge that landed. Collapsing the two is how a caller stops watching a PR that has not merged.

| Outcome | Means | Caller does |
|---|---|---|
| `Merged` | the merge landed; a `note` names any cleanup that failed around it | post-merge follow-ups |
| `Armed` | the queue owns it now | nothing; the queue merges on green |
| `Authorized` | a `decide_only` pass cleared; nothing ran | its own pre-effect step, then ask again |
| `Held` | retryable: a hold, a pending check, an already-armed PR | retry later |
| `Refused` | needs an operator: no grant, below the floor, a stale base | escalate |
| `HeadChanged` | the head moved between validation and the effect | re-evaluate |
| `Unknown` | an instrument could not answer | retry; never treat as clear |
| `Failed` | the effect ran and failed | report |

## The decision, in order

1. **One guarded fetch.** `fno do pr info` gives the number, the head, the state, and whether GitHub's queue already owns the PR. The armed flag rides that same payload rather than a second `gh pr view` probe: two fetches can describe two different heads.
2. **Terminal state.** A merged or closed PR holds before every other guard. Every guard below protects what WOULD merge, so answering "unreviewed" about a landed merge sends a caller hunting a defect that is blocking nothing.
3. **Authority.** A per-run refusal (`auto_merge_approved: false`) outranks every grant. An explicit per-run env grant (`auto_merge_source: env-target-auto-merge`) satisfies the standing arm on its own. Otherwise the LIVE config decides, so a manifest snapshot never outlives an operator flipping the switch off mid-flight. Then the automerge posture floor.
4. **The dispatch hold** (`fno do pr hold-check`), fail-closed.
5. **The in-flight review hold** (`fno do pr review-hold check`), fail-closed. Coverage answers what verdicts EXIST for a head; it cannot say a review is executing right now with its findings uncommitted.
6. **The pin.** The covered head, from the caller's own coverage gate when it has one, else from the `review_coverage` journal. An unreadable head is `Unknown`: there is no unpinned fallback. A head that no longer matches the PR's is `HeadChanged`.
7. **Base lineage** (`fno do pr base-lineage-check`), fail-open. Refusing on a gh hiccup turns auto-merge into something that silently never works, which reads exactly like nobody opting in.
8. **Checks**, when the caller asks for them. `--auto` IS waiting for the checks, so the arm path leaves this off and lets the queue enforce them server-side.

## The effect

`gh pr merge <n> [--auto] --<strategy> --match-head-commit <head>`.

The pin is never optional. It is the last guard between the decision and a racing push: gh refuses the whole call if the head moved.

No `--delete-branch`, on either path. Its LOCAL delete fails from inside the worktree that holds the branch, which made the best-disciplined merge the one most reliably reported failed. Remote cleanup is a separate step.

A non-zero gh exit is never taken at face value. The PR's own state is re-read, because gh also exits non-zero when a post-merge step fails after the server-side merge landed. A branch another worktree holds recovers through the REST endpoint, carrying the same `sha` pin.

## Who calls it

- **`fno do pr merge`** (`cli/src/fno/pr/_merge.py`) asks twice: `decide_only` first, so its coverage status receipt is never published for a merge about to be refused, then again for the effect. The second pass re-runs the whole chain, which closes the window the first one opened.
- **`fno do pr verify --kind merged`** (`cli/src/fno/pr/_verify.py`) asks once, and re-reads the PR before reporting: this verb's name is its contract, so the receipt is not its last word.
- **`fno-agents finalize`** arms at a green terminal. What stays in finalize is what only the terminal knows: the merge-gating opt-out claim, and the optional-App review evidence.

Python reaches it through `fno.rust_binary.verb_call("authorized-merge", payload)` — the single door. An unreachable binary answers `unknown`, never a clear merge: a merge whose authorization could not be read has not been authorized.

## After a queue-armed merge

When the queue lands a merge later, no fno process is in the loop. The remote ref used to stay behind forever, because the merge verb's cleanup step never runs on that path.

The PR watcher tick already detects the confirmed merge, so it pays the step there, through the merge verb's own `_post_merge_remote_delete`. One cleanup implementation, no second watcher, no second store. It is reached only from the MERGED arm, so a pending or unreadable state never deletes anything, and it is warn-only: cleanup can never fail the merge it follows.

Local branches and worktrees keep their own lifecycle ([worktree-mechanics](worktree-mechanics.md)).

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

The gate merges a PR only after its CI tested the current main. The test is ancestry, not a timestamp. The gate compares the PR against its base (`compare/{base}...{head}`). A `behind_by` above zero holds as `ci_base_stale` only on file overlap: the newer base commits touch a file the PR changes. A behind head with no shared file merges (user ruling, 2026-10-07). An unreadable file set holds. The run-timestamp heuristic this replaced answered the wrong question. A run can start after the base tip moves yet still run at a head cut before it. An unreadable freshness read answers `Unknown`, never a merge. The gate covers `Effect::Arm` too. Main has no branch protection, so GitHub merges an armed green PR at once.

After a main merge, every open PR with older CI is stale. While the slot is free, `decide` holds the first stale PR with `ci_base_stale` and gives it the slot. The holder then has GitHub merge main into its branch (`PUT pulls/<n>/update-branch`, pinned to the head it read). That is a merge commit, never a rebase, so CI reruns against the current main. The review coverage carries over as `carried_base_sync`. A conflict or a failed call holds with the manual remedy: merge `origin/main` into the branch and push. The receipt says `merge_slot_held`. While a PR holds the slot, all others are `Held`, even with fresh CI. This keeps another merge from invalidating the holder's retest. When the holder merges or closes, turns red, takes a dispatch hold, or its 60-minute lease expires, the slot releases. A held holder can satisfy none of the other three, so a hold releases the slot. An admitted holder that arms keeps the slot until its merge lands. The 60-minute TTL covers one retest, the measured 31-minute rust-ci maximum, one 10-minute sweep tick, and margin. The lease uses `<canonical repo>/.fno/claims`, keyed `merge-slot:<base_ref>`.

## The decision, in order

1. **One guarded fetch.** `fno do pr info` gives the number, the head, the state, the body, and whether GitHub's queue owns the PR. The armed flag rides that same payload. A second `gh pr view` probe describes a different head.
2. **Terminal state.** A merged or closed PR holds before every other guard. The guards below protect a merge that has not happened yet. An "unreviewed" answer about a landed merge sends a caller hunting a defect that blocks nothing.
3. **Outside PR.** The gate refuses a head repo that is not the base repo, and a non-owner-class author. Outside is final: no law row, flag or config admits an outside PR. The refusal names the rebuild path, never an admit command. An origin read that cannot answer is `Unknown`, never a quiet clear. The section below carries the rule.
4. **Authority.** A per-run refusal (`auto_merge_approved: false`) outranks every grant, except the head-scoped operator grant below. An explicit per-run env grant (`auto_merge_source: env-target-auto-merge`) satisfies the standing arm on its own. Otherwise the LIVE config decides. A manifest snapshot never outlives an operator who flips the switch off mid-flight. Then the automerge posture floor.
5. **Node binding.** The graph must see the PR. Three keys decide, in order. A node id the branch names. A node whose own back-pointer carries the PR. A closure line in the body (`Fixes <id> [<id>...]`. The retired `Backlog-Closure:` spelling still reads). The same predicate answers the lead board's untracked warning, so the board and the gate cannot disagree. When all three miss, the merge refuses. The remedy is to bind it: pick or file the node, write the closure trailer onto the body, retry. A revert or a hotfix binds the same way. There is no bypass flag. The team merges, and not only the operator. A bypass flag is one the agents pass to themselves. The gate stays silent where the repo keeps no backlog. A graph with no node under the canonical root has nothing to bind to. A graph or a body that cannot be read is `Unknown`, never a verdict.
6. **The dispatch hold** (`fno do pr hold-check`), fail-closed.
7. **The per-PR hold**, an operator law row at `pr-hold:<owner/repo>#<n>`, fail-closed beside the dispatch hold. Held, not refused: the owner releasing it clears it. The section below carries the exact commands.
8. **The in-flight review hold** (`fno do pr review-hold check`), fail-closed. Coverage answers what verdicts EXIST for a head. It cannot say that a review runs right now with its findings uncommitted.
9. **The pin.** The covered head comes from the caller's own coverage gate, or from the `review_coverage` journal. An unreadable head is `Unknown`. There is no unpinned fallback. A head that no longer matches the PR's is `HeadChanged`.
10. **Base lineage** (`fno do pr base-lineage-check`), fail-open. A refusal on a gh hiccup makes auto-merge silently never work. That reads exactly like nobody opting in.
11. **Merge result** (`fno do pr merge-result-check`), fail-open. The merge tree is computed locally and the repo-wide ruff + mypy step runs on it. The same merge tree also runs the repo's preamble budget from its own copy. Two PRs that each pass their own base can no longer merge into a main over the byte ceiling. When git joins hunks that never met on one machine, two green parents merge red. Held, not refused: the remedy is rebase, fix, push, retry.
12. **Checks**. The caller asks for these or leaves them out. `--auto` IS the wait for the checks, so the arm path leaves them out and the queue enforces them server-side.

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

## Outside pull requests and the owner's per-PR hold

The predicate lives in `pr_admission.rs` (`outside_reason`). When the head repo is not the base repo, the PR reads outside. A deleted fork reads outside. When the `author_association` is not `OWNER`, `MEMBER` or `COLLABORATOR`, the PR reads outside. Bots read outside like any other non-member author.

No outside PR is merged, healed, bound or reviewed by fleet lanes, and no law row, flag or config admits one. The refusal names the rebuild path, never an admit command. Worth-pursuing work enters through its linked issue as a backlog node. Normal triage decides. The fleet builds accepted work on its own branch and credits the contributor with a `Co-authored-by: <login> <numeric-id>+<login>@users.noreply.github.com>` trailer. The issue-first comment and intake flow (the PR steward, a separate delivery) is the contributor's way in.

The decision reads the two gates in the order above: `outside_pr` refused (and `Unknown` on an unreadable origin read), `pr_hold` held. The preview walk carries the same two as blockers, so `fno do pr status` never says ready where merge refuses. `Effect::Merge` and `Effect::Arm` both pass through the decision, so finalize's auto-merge arm and the daemon's grant queue obey them with no change of their own.

The per-PR hold is an operator law row at `pr-hold:<owner/repo>#<n>` with decision `do not merge this pull request`. It is not head-scoped, so it survives pushes, and it applies to every PR, the owner's own included. Hold:

```
fno backlog decide 'pr-hold:<owner/repo>#<n>' 'do not merge this pull request' --authority operator
```

Release by superseding it:

```
fno backlog decide 'pr-hold:<owner/repo>#<n>' 'release' --authority operator --supersedes <decision id>
```

Recording a hold disarms an armed auto-merge queue at once: the decide door runs `gh pr merge <n> --repo <owner/repo> --disable-auto` after the ruling lands. If that call fails, the ruling still stands (the record exits 0) and stderr names the manual command.

The origin facts never change for a PR: head repo, base repo, author association, login. The outside probe caches them per PR at `cache/pr-origin` inside the state root. One GitHub call per PR, ever. No TTL. Deleting the file forces a fresh read. That is also the remedy for an author whose association changed after the first read.

Heal skips every outside PR with a `skip_outside` receipt, fail closed on an unreadable origin read. Heal pushes to origin, the base repo, so it can never fix a fork's branch anyway. `guarded_push` refuses the protected branches (`main`, `master`, `develop`, `dev`) even under a force flag.

Both open-PR binding legs and the board's mergeable queue skip a row whose `isCrossRepository` reads true. Reconcile never binds a fork PR. Pr-watch, the grant queue, the review lanes and the board never see one. Only a positive true skips. A row without the field classifies as before.

Uncovered cases, kept on purpose: a hand bind of a fork PR with `fno backlog update <id> --pr-number` still cannot merge, but nothing gates a review-fix dispatch onto it. The owner's raw `gh pr merge` is the owner's call. A same-repo branch from a non-member bot is caught by the merge and heal predicate but not by the binding legs, which read cross-repo only. A member who lost access keeps reading inside until the cache file is removed. A useful bot PR can never merge through fno. The owner merges it by hand.

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

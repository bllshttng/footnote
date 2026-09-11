# The unfinished-work report and the SessionStart row registration

Two module contracts moved out of their files under the file-budget gate's remedy (long prose lives in docs, modules ship code). Content unchanged.

## The unfinished-work report (`fno.agents.unfinished_work`)

The unfinished-work report answers the operator question the fleet watchdog serves: the verdict classifier in `fno.agents.watchdog` stays the internal recovery engine (wake, reroute, reap, retire), while this module answers "was work started and never finished?" It carries four dimensions, each finding naming the one verb that clears it. `started_free_claim`: an in_progress node whose claim is free, ranked by idle age, with the branch's commits ahead of `origin/main` where a worktree resolves; clear with `/fno:target <node>`. `done_ahead_of_main`: a done node whose worktree branch still carries commits ahead of a freshly fetched `origin/main`; clear with the stranded recovery verb scoped to the repository. `dirty_ownerless_worktree`: a worktree with uncommitted paths and no authoritative live owner; clear by adopting or finishing it. `open_pr_ownerless`: a PR open past 24h whose node has no live owner; clear with `/fno:pr check <number>`.

Liveness is read only from pid incarnation and transcript truth, the authorities the fleet already trusts. A stored status word, registry absence, or display name contributes no liveness verdict: unreadable evidence preserves the candidate as unmeasurable (the dimension reads unknown, never clean), because the cost of guessing wrong is somebody's uncommitted work.

The commit metric is `git rev-list --count origin/main..HEAD` after one `git fetch origin main` per repository. The upstream tracking ref is not a substitute on this path: a stale remote-tracking ref inflated a measured count to 936 against a true 8, and a report that can be wrong by two orders of magnitude on its first line is untrustworthy. The stranded-worktree module's own unpushed probe is untouched; it protects destructive cleanup and answers a different question.

The main worktree of each repository is excluded from the dirty dimension: the canonical checkout is a shared surface with transient tenants (operator scratch files read as dirt), so the owner join cannot answer for it. `classify()` is pure over injected observations; `collect_observations()` is the IO seam; `build_report()` is the one producer both the manual verb and the scheduled tick consume.

## SessionStart row registration (`fno.agents.register_session`)

Invoked by `hooks/register-session-start.sh` as `python3 -m fno.agents.register_session --harness claude ...`. Two modes, selected by `--agent-self`. Without it, register an operator-started session (it has no row yet). With it, restamp a footnote-spawned worker's existing row, named by `FNO_AGENT_SELF`, onto the session id its harness is actually using: the id footnote passed at spawn is not durable, and registration keys its upsert on that same id, so a re-minted worker routed through registration would gain a second row rather than have its first corrected.

Fail-soft by contract (US7 AC7-ERR): any failure emits a `session_register_failed` / `session_restamp_failed` warning event and still exits 0, so the hook never blocks session start even when the registry is locked or unwritable. On success it emits `session_registered` / `session_id_restamped` and prints a one-line stderr note (hook stdout is reserved for the session preamble).

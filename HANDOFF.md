# Handoff: the verb-surface cut

This branch is a relay. One PR, one branch, no break-out.
Read this before touching anything, then delete this file in the commit that finishes the work.

## Where the count stands

| | leaves |
|---|---|
| true floor at branch point | 376 |
| after the duplicate capture alias | 367 |
| after four fully-dead groups | 355 |
| target | under 100 |

The five verbs that made 371 into 376 were live and unbaselined.
`mux layout apply`, `mux layout graft`, `mux pane split`, `mux pane break`, `mux block annotate`.

## What is left, in order

1. **29 dead leaves.** `python3 scripts/diagnostics/verb-callers.py --dead` regenerates the list. Do not work from a stale copy: the list moves as the corpus moves.
2. **The flag-shaped collapse.** Leaves whose name is a flag in disguise: the `-check` family, `lane-`, `-self`, `-json`, `-all`, `force-`. `claim` goes from 11 to 6 with nothing lost. Audit for the semantic ones too, since `pr evidence-required` sits beside `pr evidence-check` as one verb with two modes and nothing in either name says so.
3. **`fno lint` from 8 leaves to 1.** `flock-pattern`, `provider-stderr-merge`, `shellout-drift`, `spawn-paths`, `stale-skill-refs`, `verb-ratchet` are CI-only. `fno lint <name>` is one leaf where six stand.
4. **The fold-out groups.** Whole groups that do not belong on `fno` at all.
5. **The rewrite.** Roughly 137 referenced leaves, each with callers in skills, docs, and hooks that must be rewritten with it. `backlog` to about 15, `agents` to about 12, `mail` to about 6.

Steps 1 to 4 delete without rewriting a caller.
Step 5 is a rewrite of the skill and hook layer, and it is not a deletion pass.

**Pause the live fleet before step 5.** Workers call `backlog`, `mail`, and `agents` verbs continuously.

## The instrument was wrong four times

This is the most useful thing this run produced, so it goes first among the warnings.

`verb-callers.py` is the tool that authorises a deletion.
It was falsified four times before its answers could be trusted, and every falsification was a live verb sitting in the dead set.

1. **The AST walk did not strip the binary token.** `["fno", "mail", "send"]` resolved against no leaf and read as uncalled. Caught by the tool's own per-signal control.
2. **`hooks/helpers/init-target-state.sh`** invokes `fno target resolve-owned-identity` inside `$(...)`.
3. **`skills/pr/SKILL.md`** invokes `fno pr evidence-required` through the `candidate_fno` wrapper, also inside `$(...)`.
4. **`scripts/lib/eval-sweep-throttle.sh`** invokes `fno loops status` through a shell VARIABLE holding the binary.

Cases 2 to 4 share one shape: the binary name is glued to a variable assignment inside a single whitespace-delimited token, so stripping outer punctuation left it unrecognised.
All three verbs are live. One is a hook.

**All four are enforced controls now.** A tokenizer change that loses any of these shapes emits no candidate list at all, rather than a wrong one.

### The rule this bought

A zero is a measurement only when the instrument demonstrably finds a caller where one exists.
Absence of a literal hit is not evidence of absence.

Confirm the dead set against a SECOND method before deleting.
`/Users/bb16/.claude/jobs` is gone with the session, so the throwaway script is not here, but it is fifteen lines: walk every file in the repo, look for the literal verb path, ignore the corpus rules entirely, and assert a known-live verb produces hits.
Two methods sharing no code is the point.

## The tombstone pattern

A verb deleted outright fails its callers with the same message a typo gets.
Every removal in this branch names its replacement instead.

- `cli/src/fno/tombstones.py` holds the table and the group class. `crates/fno/src/main.rs` holds the Rust equivalent (`MUX_TOMBSTONES`).
- A group that can lose verbs takes `cls=tombstone_group_cls("<its user-facing path>")`.
- **Hand the group its path; never infer one.** Two rounds of inference each shipped a tombstone that answered confidently for a verb nobody typed. Keying on `ctx.command_path` missed direct invocation, because `fno backlog`'s own Typer name is `graph`. Falling back to a suffix match then let a bare `fno inbox` answer for `backlog inbox`, and let `fno backlog log` answer for the top-level `log`.
- **Assert the typo path beside every tombstone.** A table that swallowed every unknown verb would pass a tombstone test and hide real typos behind a confident wrong answer.
- A tombstone resolves to a refusal, never a command, so it adds nothing to the baselined surface.

## The ratchet, and why it landed first

It could not fail for the reason it existed.
A hand-typed Python tuple was compared against a hand-typed Rust usage string, both written together, both omitting the same verbs.

It now reads two independent sources that must agree: the match arms and equality guards in `crates/fno/src/`, and the alternation the live front prints when it refuses a bogus verb.
Disagreement either way fails closed.

**Verify it behaviourally, never by asserting a known verb is present.**
Add a throwaway match arm to `mux_cli.rs` and confirm the ratchet goes red naming the arm.
An assertion that the scan contains `mux pane ls` passes over a hardcoded list and proves nothing.

The surface grew by four leaves during the planning of this cut.
That is the argument for the instrument landing before the deletions, and it is why this file exists rather than a larger unfinished diff.

## Where the budget ran out

At 355 leaves, after four commits of cutting and two of instrument work.
Nothing is half-applied. Every commit is atomic and the tree is green.

Local suite at that point: 12003 passed, 115 skipped, 0 failed. Rust: all green.
A self-review ran on the final head and its findings are closed, including one it had skipped as latent.

The remaining work was not started, deferred, or carved out.
It is simply not done, and the ordering above is the ordering to do it in.

## One red you will meet, and it is not yours

`cli/tests/unit/test_graph_sidecar_window.py::test_ac3hp_concurrent_writes_never_surface_corruption` failed once in the `smoke` and `smoke-dirty` jobs on this branch.

It is not caused by anything in this cut.
No commit here touches the graph store, the sidecar, or any locking path.
It passes locally, 11 of 11, and its own history is a run of load-sensitivity fixes: widening the concurrent-write yield for loaded runners, and stopping reader threads from GIL-starving the writer.

The measurement, for whoever picks it up: one `GraphCorruptionError` in 1254 reads, while a writer held the lock for 200 mutations.
Two sibling readers reported the same single hit out of 2312 and 1158 reads.
The readers do not take the lock, so this is the atomic-write window being observed rather than a test artifact.

That is a real, rare product finding and it deserves its own node.
It is recorded here rather than fixed because fixing an atomic-write race is not a verb-surface change, and burying it in this branch is how it gets lost.
Do not "fix" it by widening the yield again.

## Two corrections carried in the PR body

`gh` has 42 top-level groups and 205 leaf commands, measured the same way with a positive control.
The comparison that produced "gh 39" put gh's GROUP count against our LEAF count.
We are 1.8x gh, not 9.4x.

`backlog` owned 93 leaves at branch point, not 91.
That is more than gh's `repo`, `project`, and `pr` combined, which is 62.
The ratio inside one group is the real indictment.

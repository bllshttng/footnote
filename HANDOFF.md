# Handoff: the verb-surface cut

This branch is a relay. One PR, one branch, no break-out.
Read this before touching anything, then delete this file in the commit that finishes the work.

## Where the count stands

| | leaves |
|---|---|
| true floor at branch point | 376 |
| after the duplicate capture alias | 367 |
| after four fully-dead groups | 355 |
| after 26 dead leaves | 329 |
| after `fno lint` 8 -> 1 | 322 |
| after merging origin/main (which added 2) | 324 |
| after the claim collapse | **321** |
| target | under 100 |

Regenerate the count with `uv run --project cli fno-py lint verb-ratchet`.
Run it that way, never as a bare `fno`: a bare `fno` enumerates the INSTALLED package while writing this checkout's baseline, and the lint refuses that combination rather than emitting a byte-identical file plus a success line.

## What is left, in order

Steps 1 to 3 of the original plan are DONE. What remains is the rewrite, and one contained collapse.

1. **`fno pr evidence`.** `pr evidence-check` and `pr evidence-required` are one verb with two modes. Fold to `fno pr evidence [--required --base <ref>]`, where the bare form keeps the exit-code contract and `--required` keeps the JSON policy line.
   Callers: `skills/pr/SKILL.md:101,110`, `scripts/ci/preflight.sh:254`, `tests/ci/test_preflight.sh:65,195`.
   **This one touches an enforced control.** `scripts/diagnostics/verb-callers.py:150` pins `"pr evidence-required": 1` as one of the four controls that exist because the instrument was falsified. Update the control to the new verb path IN THE SAME EDIT, and confirm it still finds the `skills/pr/SKILL.md` caller inside `$(...)` afterwards. A control that stops finding its caller emits no candidate list at all, which is the designed behaviour, so a mistake here is loud rather than silent.
2. **THE REWRITE.** `backlog` 73 -> about 15, `agents` 40 -> about 12, `mail` 16 -> about 6. That is roughly 96 leaves and it is the whole remaining distance to under 100. Every naming skill, doc, and hook gets rewritten with it.

### Folding the top level comes AFTER the cuts (operator decision, 2026-08-12)

Folding a top-level group under another moves a NAME; it does not cut a leaf.
`fno codemap` becoming `fno backlog codemap` is still one leaf.
Measured at 321 leaves across 58 top-level names: 25 of those names hold exactly one verb (so folding them gains nothing), 19 hold 2-4, and 14 hold 237 of the 321.

There are no free leaves left up there.
Ten top-level names collide by name with a nested leaf (`done`/`backlog done`, `whoami`/`agents whoami`, `restart`/`agents restart`, `status`/`agents status`, and six more), which is the shape that made `backlog inbox` worth nine free leaves.
Every one of the ten resolves to a DIFFERENT callback, so none is a duplicate registration.
That check is `iter_python_leaves()` compared on `cmd.callback.__wrapped__`; re-run it after the rewrite, because the rewrite is what could create a true alias.

So fold the top level for coherence, sequenced AFTER the rewrite.

### The rest of step 4 is not a deletion, and this is measured

The relay brief said steps 1 to 4 delete without touching a caller.
That is true of steps 1 to 3 and false of step 4, and the operator has accepted the correction.

Every group on the fold-out list has live callers. Measured on this branch:

```
target   8 of 9 leaves called   (target init alone is 86 refs, a positive control)
plan     10 of 11               (rung 26, path 21, validate 15)
paths    4 of 4                 (shell-stub 20)
done 47   review 27   retro run 23   status 20   codemap 20   phase 16   notify 16
pr-watch 6/6   route 4/4   roles 4/4   carveout 4/4   approvals 3/3
```

So folding those groups is a rewrite of the same nature as step 5, not a free delete.
Sequence it with the rewrite, not before it.

## The fleet does not need pausing

The earlier condition ("mail before the rewrite commit lands") was aimed at the wrong event and the operator has withdrawn it.
`fno` resolves to `/Users/bb16/.cargo/bin/fno`, an INSTALLED binary separate from this checkout, and live workers read hooks from main.
A branch commit cannot reach them.
The risk window is **merge plus `fno update`**, so mail the operator before MERGE rather than before any commit.

## The instrument was wrong four times

This is the most useful thing this branch produced, so it goes first among the warnings.

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
The throwaway script is fifteen lines: walk every file in the repo, look for the literal verb path, ignore the corpus rules entirely, and assert a known-live verb produces hits.
Two methods sharing no code is the point.
It was run against all 29 candidates and again after merging origin/main, and it agreed both times, modulo four English-prose hits on "state path" that are not invocations.

## A zero is a candidate, not a ruling

Three leaves scored zero and STAY. This is the rule the dead set does not encode, and the next pass will meet it again.

- **`loops resume-all`** is the only release for the `pause-all` kill switch. Deleting it strands a paused fleet with a sentinel file to remove by hand.
- **`backlog queued`** and **`annotate list`** are the read-back halves of live write verbs (`backlog queue`, `annotate add`). Removing a reader while its writer stays leaves data nothing can reach from the CLI.

The test to apply: does a live verb still WRITE what this verb READ, or still open what this verb CLOSED?
If so, deleting it breaks a workflow no caller count can see.
A diagnostic over derived state (`backlog validate`, `backlog triage pile`) fails that test and goes.

## The tombstone pattern

A verb deleted outright fails its callers with the same message a typo gets.
Every removal in this branch names its replacement instead.

- `cli/src/fno/tombstones.py` holds the table and the group class. `crates/fno/src/main.rs` holds the Rust equivalent (`MUX_TOMBSTONES`).
- A group that can lose verbs takes `cls=tombstone_group_cls("<its user-facing path>")`. Ten groups carry it now. The `agents` group cannot, because it already needs its own class for Rust routing, so it does the same lookup on the same `get_command` seam.
- **Hand the group its path; never infer one.** Two rounds of inference each shipped a tombstone that answered confidently for a verb nobody typed.
- **Assert the typo path beside every tombstone.** `/Users/bb16/.claude/jobs/*/tmp/checktomb.py` is gone with the session, but it is twenty lines: for each key, run it and require "was removed"; then run the same group with `zzz-no-such-verb` and require that it is NOT swallowed. All 34 pass.
- **RUN the replacement before you write it down.** Two of this pass's tombstones were plausible and wrong. `triage health` reports the IDEA pile, not the deferred pile. `backlog triage validate` validates a proposal FILE, not the graph. Reading the name would have shipped both. Keep the clause that says where the capability did NOT go.
- A tombstone resolves to a refusal, never a command, so it adds nothing to the baselined surface.

## The ratchet, and how to verify it

It could not fail for the reason it existed.
A hand-typed Python tuple was compared against a hand-typed Rust usage string, both written together, both omitting the same verbs.

It now reads two independent sources that must agree: the match arms and equality guards in `crates/fno/src/`, and the alternation the live front prints when it refuses a bogus verb.
Disagreement either way fails closed.

**Verify it behaviourally, never by asserting a known verb is present.**
Verified again on this branch: adding `"zzz-throwaway-arm" => PaneCmd::Ls { fno_id },` to `crates/fno/src/mux_cli.rs` turns the gate red naming that arm (`mux pane dispatches verb(s) its own refusal message does not name: zzz-throwaway-arm`). Remove the arm, green again.
An assertion that the scan contains `mux pane ls` passes over a hardcoded list and proves nothing.

## Rebase, and why this branch merged instead

`fno doctor` will tell you the checkout is behind. It was 51 commits behind on 2026-08-12.

This branch took `git merge origin/main`, not a rebase.
A rebase replays 14 commits and requires a force-push to a branch that already carries PR 831, and force-pushing is barred for this session.
The merge resolved two conflicts and cost one commit:

- `cli/src/fno/target_cli.py`: main added `target denominator-ratio` in the same region this branch deleted `target resume-bind`. Both resolutions are correct and both are in the tree.
- `scripts/diagnostics/verb-callers.py`: main added `load_curriculum`, this branch renamed `iter_corpus` to `iter_paths`. Both survive.

After merging, **re-run the second-method sweep against the removed set**. Fifty-one commits are enough to add a caller to something already deleted. It was run and it stayed clean.

## Snapshot assertions are a trap in this branch

`cli/tests/lint/test_verb_callers.py` pinned `complement (untaught): 277`.
A verb cut, which is the thing that tool exists to support, failed a test that had nothing to say about the cut.
It now asserts the PARTITION (`taught + untaught == baseline leaves`, all positive), which is the real invariant.

Expect more of these as the rewrite lands. `cli/tests/unit/test_lifecycle_pairs.py` carries a `KNOWN_COMMANDS` frozenset for `backlog` that must be edited with every backlog change; that one is deliberate (a new verb should force a classification decision), so edit it, do not weaken it.

## One red you will meet, and it is not yours

`cli/tests/unit/test_graph_sidecar_window.py::test_ac3hp_concurrent_writes_never_surface_corruption` failed once in the `smoke` and `smoke-dirty` jobs on this branch.

It is not caused by anything in this cut.
No commit here touches the graph store, the sidecar, or any locking path.

The measurement, for whoever picks it up: one `GraphCorruptionError` in 1254 reads, while a writer held the lock for 200 mutations.
Two sibling readers reported the same single hit out of 2312 and 1158 reads.
The readers do not take the lock, so this is the atomic-write window being observed rather than a test artifact.

That is a real, rare product finding and it deserves its own node.
Do not "fix" it by widening the yield again.

`cli/tests/unit/test_mutex_steal.py::test_AC3_FR_concurrent_stealers_both_land` also failed once under a loaded parallel run with a lock timeout, and passed on its own immediately after. Same class, same advice.

## Two corrections carried in the PR body

`gh` has 42 top-level groups and 205 leaf commands, measured the same way with a positive control.
The comparison that produced "gh 39" put gh's GROUP count against our LEAF count.
We are 1.6x gh at 321, not 9.4x.

`backlog` owned 93 leaves at branch point, not 91.
That is more than gh's `repo`, `project`, and `pr` combined, which is 62.
The ratio inside one group is the real indictment.

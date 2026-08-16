# Native review lanes

How to actually get a diff reviewed by the harness's native review verb
(Claude `/code-review`, codex `/review`), and the constraints each
trigger lane carries.
This is the operational counterpart to [coordination](coordination.md)
(which covers who owns work) and [cross-model-review](cross-model-review.md)
(which covers the `fno` sigma/peer panels, a different surface).

The load-bearing correction this doc exists to carry: the widespread
belief that `/code-review` "cannot be self-invoked by the session that
wrote the diff" is too strong.
Self-invocation has worked for several workers, so it is the first lane
to try - but it has also been refused, so it is not a guarantee.

The obligation to use one of these lanes on a code payload is enforced at the stop gate (`crates/fno-agents/src/loopcheck.rs`) and `fno pr merge`, not only in this prose: a code PR that reaches the gate with no head-pinned `review_attestation` is held, and the held reason names this harness's verb.
This doc is the lane menu; the gate is the authority.
Opt out with `config.review.self_review_required = false`.

## Lane 1: self-invoke via the Skill tool (primary)

An in-session agent can launch the native review verb through the Skill
tool with **bare args**, not by typing the slash command:

```
Skill(skill="code-review", args="<level> --comment --fix")
-> Skill "code-review" launched (forked execution, running in the background).
```

This was confirmed by three separate workers executing it across
multiple review rounds.
The forked run found real defects that two separate hand-rolled review
passes (dispatched `fno:code-reviewer` subagents) both missed,
including one silent-data-loss bug.
Use bare args.

### Aiming it at a specific PR

The fork **inherits the session cwd**, so it reviews the ambient diff
of whatever worktree the session is in.
A shell `cd` does not move a session, so it does not retarget the fork.
To aim the review at a specific PR, put the session in that PR's
worktree with the **EnterWorktree** tool, then launch bare.
A worker seeded via `fno agents spawn --cwd <worktree>` lands there
correctly and reviews the right diff without an extra move.

## Lane 2: raw-inject via `fno mail send --raw`

The documented operator front door for asking another session (or your own) to fire a raw verb is `fno mail send --raw`.

It routes by the recipient's live lane. It either fires the requested operation or names why that lane cannot fire it.

On a prompt-line lane it injects the payload UNWRAPPED. No `<fno_mail>` envelope wraps it, so the slash sits at character 0 and parses like a human keystroke.

That sidesteps any model-invocation refusal. The prompt-line injection IS the user-invocation path.

Use it to fire a verb in a live worker, or with `--to-self` to target the current session through the same router.

```bash
# Into a peer:
fno mail send <peer> '/code-review <level> --comment --fix' --raw
# At your own prompt line (recipient derived from ambient identity):
fno mail send '/code-review <level> --comment --fix' --to-self --raw
```

`<level>` is sized from the diff by `level_for_diff` in `cli/src/fno/review_capability.py` (never `ultra`: billed separately, and the builder rejects it).

## A reply does not resolve a thread

`fno pr status` reports `optional_reviews_unresolved`, and `ready` is `green && unresolved == 0`.
Answering a finding in-thread does not decrement that counter: a GitHub review thread stays unresolved until it is resolved EXPLICITLY.
So a PR whose every finding has been fixed and answered can sit at `ready: false` indefinitely while reading, to a human and to the loop, as fully handled.

Resolve each thread with the "Resolve conversation" button, or:

```bash
gh api graphql -f query='mutation($t: ID!){resolveReviewThread(input:{threadId: $t}){thread{isResolved}}}' -F t=<threadId>
```

Thread ids come from `reviewThreads` on the `pullRequest`.
`fno pr status` prints this instruction on stderr whenever the counter is non-zero, so the fix travels with the number rather than living only here.

Before you tell anyone else to run one of those, ask whether they can: `fno mail send '<payload>' --to-self --raw --check` (or `fno mail send <peer> '<payload>' --raw --check`) injects nothing and reports one of THREE answers, never two: `injectable: <lane>` (exit 0), `not-injectable: <reason>` (exit 1), or `unmeasurable: <reason>` (exit 3) when the evidence needed to decide could not be read at all.
Branch on all three. Collapsing `unmeasurable` into `not-injectable` states a verdict about a session the run never measured, and a deployed `fno-agents` too old to carry `--probe` answers `unmeasurable: probe-unavailable`.
It answers whether a PATH exists, never whether the turn lands, since no probe can see whether the prompt line is idle.
See [mail-live-inject](mail-live-inject.md) for what it resolves and why the Stop hook gates its compact advice on it.

`fno mail send --raw` routes to the right transport per recipient, and that transport is not always the same binary.

A mux-hosted session injects via `fno mux pane send`, regardless of harness. A mux-hosted Codex session is therefore a real prompt-line lane. It can fire `/compact`, `/review`, or any other TUI verb the Codex parser accepts.

A Claude daemon session injects via the `fno-agents mail-inject` Rust binary (`cli/src/fno/agents/dispatch.py`).

A Codex app-server thread has no prompt line. The Python front door routes the exact verbs `/review` and `/code-review` to the structured `review/start` RPC. It refuses every other raw payload, naming the app-server constraint and the wrapped-send or mux alternatives.

Other non-keystroke daemon lanes keep the generic refusal. A slash submitted through their model-turn RPC arrives as text and does not fire.

`fno mail send --raw` is therefore the single documented raw-payload entry point. The `mail-inject` and `review-start` binaries remain low-level structured or STDIN expert doors, not alternative raw-payload routers.

The `mail-inject` binary remains reachable directly for scripting against a Claude daemon session outside the Python CLI, where its STDIN form suits a pipe:

```bash
printf '/code-review <level> --comment --fix' | fno-agents mail-inject --harness claude --session <full-session-uuid>
```

It reads the turn text from STDIN and enforces the brevity cap for a direct binary call. The raw lane itself is capped in Python at the `fno mail send --raw` front door. The binary holds the same ceiling for callers that bypass it.
A shared `FNO_MAIL_BODY_WARN` / `FNO_MAIL_BODY_REFUSE` knob pair keeps the threshold identical to the wrapped-mail cap, so the direct binary is not a way around it.
The cap skips framed envelopes: a `<fno_mail>` body is already capped in Python before it reaches here, and a `<cross-session-message>` relay hop is internal traffic, not authored mail, so neither is refused here.
An over-cap unwrapped body is refused before it is delivered; the STDIN form is for piping the turn, not for moving a verbose payload.

`--session` takes the full session UUID or its 8-hex short id (the roster accepts either). This raw/direct form targets the Claude control-socket lane.

It delivers over the daemon `control.sock` to a live `claude --bg` session, so the target must be an adopted live session: it never lazy-starts one.

The binary's `mail-inject --harness codex` mode submits text through `turn/start`. It is the wrapped-mail transport, not a raw verb transport, because the Codex app-server has no slash parser.

The structured Codex expert door is `fno-agents review-start --session <thread-id> --target <uncommittedChanges|baseBranch:branch|commit:sha|custom:instructions> --delivery inline`.

That direct verb takes an already-structured target rather than a raw payload. The exact-verb allowlist stays Python-only at the raw-payload door, so no second raw-payload door opens in Rust.

When the target repository has no `refs/remotes/origin/HEAD`, a bare `/review` or `/code-review` on a Codex app-server thread refuses and requires `--base`. The router derives `baseBranch` only from that authority. The `--uncommitted` flag remains the explicit working-tree target.

**Discoverability note.** `mail-inject` is a `fno-agents` *binary* verb, not a `fno mail` or `fno agents` (Python CLI) verb.
It is matched with `matches!` in `crates/fno-agents/src/bin/client.rs`, deliberately, so the routable-verb parity guard does not see it; that keeps it out of `--help` and `CLIENT_VERB_USAGE`.
So `fno mail --help`, `fno agents --help`, and a grep of the Python tree all report nothing, and a "does this exist?" probe against any of them answers false.

When its explicit structured or STDIN contract is the point, use the hidden binary verb. Otherwise reach for `fno mail send --raw` for recipient-aware routing.
Do not conclude the lane is absent from an empty `--help` or an empty Python-tree search; the binary verb is there.

## Lane 3: king-mediated mail (fallback)

When neither self-invocation nor a raw inject is available - no live
session to inject into, or a worker's harness lacks the verb - ask a
king over `fno mail send`.
The king's reply injects as user-shaped text and the worker's own
harness serves the verb in response, or the king can fire the verb
into the worker's live session directly via
`fno mail send <worker> '<verb>' --raw` (Lane 2).
With no live king, fall back to advisory self-review or run the native
verb by hand.

## Why (wrapped) mail cannot carry a verb

A wrapped `fno mail send` cannot carry a verb. It writes an `<fno_mail ...>` envelope at character 0 of the input. The slash command therefore never sits at the start and never parses. Mail carries **instructions** ("review my diff"), not invocations (a sized `/code-review` order). `--raw` (Lane 2) is the deliberate exception. It strips the envelope so the slash parses, and that is the cost the wrapper exists to impose.

## Do not assert a cause for a refusal

A `disable-model-invocation` refusal has been observed, and a
PR-number argument (`args="medium --fix <n>"`) was refused once.
**Neither has a confirmed cause.**
Do not invent a mechanism for a refusal you see, and do not instruct a
worker to check a flag before invoking.

One proposed cause, an `enabledPlugins.code-review` config gate, was
**falsified**: a worker launched cleanly with both
`code-review@claude-code-plugins` and `code-review@claude-plugins-official`
still `false` at `~/.claude/settings.json`.
Do not repeat that theory.

In one observed window the "a worker can execute it" premise did not
hold at all: a main background session and a freshly spawned background
worker were both refused with the identical `disable-model-invocation`
text, and the worker retried many times with no findings.
So the refusal can be environment-wide across session types in a given
window, not a property of one session's arg shape.
The refusal text names the escape: it applies to MODEL invocation, and
`fno mail send --raw` (Lane 2) is the user-invocation path that lands the verb
as user-role text, so it is not subject to that refusal - reach for it
when self-invocation is refused.
The one environment-wide window predates the raw-inject lane's verification
(confirmed separately, the next day) and was not exercised there, so
treat that window as open; if `fno mail send --raw` fails it too, report the
exact refusal text and surface it to a human rather than burning cycles
re-invoking.

The standing lesson: every plausible mechanism proposed for this verb's
refusals has so far been falsified by a worker executing it.
Guard the value, not a correlate.

The Skill-tool success record (three workers) and a self-initiated
refusal record sit side by side, and no cause has held up.
`fno mail send --raw` (Lane 2) is the most reliable trigger and the one to
reach for when self-invocation is refused: it is the user-invocation
path, so the model-invocation refusal does not apply to it.
Short of that, the king-mail loop fires often but not always (refused
twice in one session with an order in hand).
Treat self-invocation as the lane worth trying first, `fno mail send --raw` as
the reliable fallback, and king-mail as the asynchronous one - not a
closed either/or.
The king-mediated path, the per-harness verbs, and the never-substitute-
silently contract have a deeper treatment in
[king-for-a-day/references/review.md](../../skills/king-for-a-day/references/review.md).

## Counting invocations

Counting how often a skill was invoked by the `<command-name>` marker
gives a **floor, not a count**.
The harness emits `<command-name>` only for a *typed* slash command; a
Skill-tool call emits a `tool_use` record instead, which is invisible
to that probe.
Every "0 parsed" reading from a `<command-name>` count is accurate and
irrelevant: it guarded a correlate and concluded from its absence.

Grepping the skill *name* is worse, not better: the skill list is
injected into every SessionStart preamble, so a bare name grep matches
thousands of transcripts that merely mention the skill.
A correct count unions a `<command-name>` probe with a `tool_use` probe
for the skill name, and uses the counting session's own id as a
positive control (it must find at least itself).
This shape is general to any programmatic skill invocation, not just
review: counting king-for-a-day reigns by the `<command-name>` marker
undercounts the same way, since a reign fired through the Skill tool is a
`tool_use`, not a typed command.

## Review freshness: one predicate, both producers

A review verdict is evidence about a COMMIT, not about a pull request.
Deciding whether it still applies used to happen twice, with two different rules, and neither was right.

The `github_app` axis had no rule at all.
A bot verdict counted on any non-empty `state`, and nothing asked which commit the bot had read.
On PR #826 codex submitted its review against `8e557ccd` at 17:51:48Z.
The coverage event at 19:09:43Z read `head_sha 89bc0b91, coverage covered, reviewed_count 2`.
`89bc0b91` is a commit codex never saw.
The same shape appears on #827 and on #824, where the inherited verdict was twelve hours old.

The `local_attestation` axis had the opposite failure.
It used a bare `head_sha == HEAD` equality, with no gradations.
Addressing a review invalidated the proof the review happened.
An agent given three findings, fixing them in three commits, owed three re-reviews.
Across footnote PRs 824-831, PR 828 moved through six heads and PR 830 through five.

Both now call one predicate, `review_freshness(reviewed_sha, head_sha)`.
It lives in `crates/fno-agents/src/loopcheck.rs` and returns one of four states:

- `fresh` - the reviewer read this exact commit.
- `carried_base_sync` - the PR's own code delta is byte-identical. Any tree difference came from the base moving. A rebase is this shape.
- `carried_docs_only` - only documentation paths changed since the reviewed commit.
- `stale` - everything else, **including every failure path**.

`carried_*` is decided by comparing a **PR code-diff identity** at each commit.
That identity is the diff from `merge-base(base, sha)` to `sha`, documentation paths dropped, hashed.
Equal identities mean the code under review is byte-identical, whatever happened to the sha.
That is what makes a rebase carry and a one-line code fix die.

`reviewed_sha` comes from a different place per producer, and both were already available.

| Producer | `reviewed_sha` source | Cost |
|---|---|---|
| `github_app` | the review object's `.commit.oid` | none, already in the `gh pr view --json reviews` payload |
| `local_attestation` | the attestation's own `data.head_sha` | none, already emitted |

Three separate places ask "has this reviewer reviewed this code", and all three go through the predicate.
They are the coverage count, the `config.review.reviewers` attestation scan, and the required-bot presence check that drives `missing_bots`.
Fix one and leave the others on a bare equality, and the gate stays exactly as tight as before.
The softening is then purely decorative.

A `stale` verdict is **recorded, not dropped**.
`CoverageVerdict::Stale` says a reviewer responded against an older commit.
That is a different fact from `absent`, and it calls for a different response: ask for a re-read, rather than wait for a first read.

### What the carry rule does not buy

It does not deliver relief from the re-review treadmill, and the measurement says so plainly.
Of the 22 head-to-head transitions observed across PRs 824-831, **2 carry forward**.
The other 20 are genuine code change, measured against each PR's true base.

An earlier pass measured 63% and was wrong.
Merged PRs' three-dot diff against the current `origin/main` is empty, and the hash of the empty string equals itself.
Twelve transitions were matching an absence against an absence.
So a carry requires a positive match between two SUCCESSFULLY COMPUTED identities.
An empty code diff yields no identity at all.
`freshness_two_absent_identities_never_match` is the standing guard.

What the rule does deliver is rebase-invariance.
It fires at the mandatory pre-merge rebase, where losing an attestation costs most.
That is what makes the `github_app` tightening survivable.
It is not "five re-reviews become one".

**An unused-import removal still costs a full re-review.**
`fix(tracker): drop unused json import (ruff F401)` changes a `.py` file.
That is code under any classifier that does not parse Python, and an AST dependency for one commit shape is not worth it.
A documentation-only PR never carries an attestation either.
With no code in the diff there is no identity to match, which is the fail-closed direction.

**`carried_docs_only` inherits `is_documentation_path`, and that classifier calls every `.md` file documentation.**
In this repo `skills/*/SKILL.md`, `agents/*.md`, and `AGENTS.md` are behavior, not prose.
So a skill rewritten after a review carries the earlier verdict forward as fresh coverage.
This is deliberate for now, because it matches the existing payload classifier.
A `.md`-only PR already skips review gating entirely, so the carry rule is not what introduced the gap.
Narrowing it is a real behavior change and has to move in lockstep with the Python mirror in `_merge._is_documentation_path`.

### Named, not closed: the derivation-latency window

A valid attestation can exist while the gate cannot see it, and this PR does not close that.

Raw `review_attestation` events never leave the checkout they are emitted in.
`scripts/setup/setup-worktree.sh` links `.fno/config.toml`, `.fno/carveouts.jsonl`, and `.fno/codemap.md` into a worktree.
`events.jsonl` appears zero times in that script, so a worktree's event log is a real per-checkout file.
Only the DERIVED `review_coverage` aggregate reaches the global `~/.fno/events.jsonl`, written by the Rust runtime.
So the propagation path is the aggregate, and the aggregate only exists after a loop-check run.

PR #830 is the specimen.
Its attestation was emitted at 04:12:35 into a worktree-local log.
The canonical gate read `reviewed_count 0` until the Rust loop-check re-derived the global aggregate at 04:15.
Between emit and that run, the gate reads a stale aggregate for a PR that is genuinely reviewed.

It is named here rather than fixed because both available fixes are worse than the window.
Emitting raw attestations globally changes where a per-checkout log lives, which is propagation architecture and not freshness.
Having the Python reader walk raw attestations puts a second implementation of the coverage computation beside the Rust one.
That is the two-divergent-rules disease this document exists to cure.
The window is bounded by the loop-check interval and fails in the safe direction.
The gate under-reports coverage and holds. It never over-reports and merges.

### What an attestation proves

It proves a commit was pinned. It does not prove a review happened.

Every producer bottoms out in the same script, `skills/review/scripts/emit-attestation.sh`. It records the reviewer name and the verdict PASSED.

sigma calls the script itself, from inside its own skill, on a clean pass. On Claude Code, `hooks/code-review-attest.sh` calls it for `/code-review` too.

That hook is wired on two events, because `/code-review` reaches a clean pass two different ways. A `PostToolUse(ReportFindings)` pass fires the hook directly. A Skill-tool self-invocation runs `/code-review` as a forked subagent, whose verdict never reaches ReportFindings, only its final text. A `SubagentStop` trigger reads that text instead, so the second path also fires the hook.

Either trigger fires the moment the verb reports an empty findings array. The caller runs no second command.

When neither hook can fire - codex `/review`, or the registered-reviewer case - the script still gets called directly. `/target` runs the review verb, then runs the helper by hand.

Nothing in the producer can tell a real review from a caller that typed the arguments.
The freshness half of the protocol is sound and is only half.

Two consequences are recorded rather than gated.
`self_attested_count` on the coverage event says how many of `reviewed_count` are the author attesting its own diff.
It is recorded rather than gated because self-review is the DEFAULT path, and refusing it wedges every single-session PR.
The `model` field records what the environment CLAIMED.
The producer refuses to record a claim it can prove false.
A non-`claude*` model name with no non-Anthropic base URL cannot be the model that answered.
A refused claim stores the literal `unobserved`, never an empty string.
So a claim that was made and declined never reads the same as a field nobody set.
A claim that is merely unverified still records as given, because nothing in a shell can check it.
That refusal landed separately.
`tests/hooks/test_attest_model.sh` drives the hook and the emitter over one env matrix, so the two predicates cannot drift.

## Attestation origin: whose process rendered the verdict

A local attestation records `attester_session_id`, the harness session of the
process that emitted it, read from the live environment on the same marker
precedence `fno target init` resolves.
loop-check compares it against the authoring session's manifest
`harness_session_id` and labels each local verdict with a tri-state
`attestation_origin`:

- `self_attested` - the authoring session emitted the attestation.
- `other_session` - a different session emitted it.
- `unknown` - no attester was recorded, or the author session is unknown.

`other_session` is not `independent`.
The manifest names the session that ran `fno target init` in the worktree, so a
self-handoff successor or a second agent in a shared worktree is a different
session and is still not independent.
A match is strong evidence of self-attestation; a mismatch is weak evidence of
anything.

The origin is recorded, not gating.
`reviewed_count` never consults it: every `reviewed` verdict counts toward
coverage regardless of its origin, `self_attested` included.
What the coverage event now adds is `self_attested_count`.
How much of the count is the author reviewing itself is a NUMBER a reader can see, rather than a fact stated in prose.
It is deliberately not called `independent_count`, for the reason given just below.
A gate that wants to demand a second reader is one predicate over that number.
That is a merge-authority decision, tracked separately.

**A green PR whose only attestation is `self_attested` is covered. Merge it.**
`self_attested` is not a hold condition and has never been one.

**The spawned-reviewer lane.** A non-self attestation is producible today: the author spawns its own reviewer citizen, a different session by construction, so its attestation renders `attestation_origin = other_session`.

```bash
fno agents spawn --name <name>-review "/code-review <size> for PR <n> against main" \
  --harness claude --substrate bg --model opus \
  --permission-mode bypassPermissions --cwd <an isolated reviewer worktree>
```

The reviewer worktree must run `scripts/setup/setup-worktree.sh`. That script symlinks its `.fno/events.jsonl` to the repository's canonical journal.

The shared journal lets the reviewer stay isolated from the author's files. Its exact-HEAD attestation is still visible to the author's loop-check.

A `--fix` that touches only documentation now carries rather than invalidates. The freshness rule is therefore not the reason this constraint stands. The tree-corruption specimens are.

NO `--fix` remains the review contract. The author applies findings and re-attests. The reviewer's prior attestation is then stale by design.

Two worktrees at the same exact HEAD can see each other's attestations. Session identity stays part of the coverage origin, and HEAD movement invalidates the shared evidence.

The lane also buys cross-model review, which the king-mediated lane cannot: a GLM or codex author spawns a claude reviewer (or vice versa), so "different session" can mean "different model".
The identity scrub on every spawn substrate is what makes a cross-harness reviewer stamp its own session rather than the author's; without it the lane's headline value, `other_session`, is silently unreachable.

The king-mediated lane (Lane 3) still cannot produce independence by construction: it fires the review verb at the worker's own prompt line, so the author runs and emits it.
That lane produces compliance, not independence; the spawned-reviewer lane is what produces the latter.

Two workers held green PRs on 2026-08-07 waiting for a second attestation that no dispatched lane emitted then, and escalated to the operator to merge on their behalf; neither was blocked.
The spawned-reviewer lane is the path that did not exist for them.

No gate lands with the lane.
Producing a countable non-author attestation and gating on it are separate decisions; `self_attested` stays a recorded origin, never a hold condition.
"Land, measure, then decide" no longer measures zero percent independent forever, because the lane above is what emits the `other_session` value the sequence was waiting on.
Whether to hold a green PR on a self-attested-only attestation remains its own decision, tracked on its own.

This records WHOSE process rendered a verdict; the role-routing note in
[role-based-model-routing.md](role-based-model-routing.md) records WHICH model,
and states the claim this makes measurable: keep the reviewer off the authoring
worker; a role table cannot enforce it.

## Producer reachability: every path that reaches the gate

`fno pr merge` refuses a PR whose `review_coverage` event says uncovered, unknown, stale, or missing. For most of the gate's life exactly one writer existed: `read_pr_info` under `run_done`, which `decide()` reaches only past a streak counter. A session with no `.fno/target-state.md` runs no stop hook at all, so no row will ever exist. The gate was unsatisfiable for that session shape, not strict. This is the decorative-guard pitfalls entry inverted: a PRODUCER on one of N paths.

The producer is now reachable from every path that can reach the gate.

| Path | Producer reachable | How |
|---|---|---|
| target stop hook `decide()` -> `run_done` | yes, unchanged | streak-gated |
| `fno pr merge <n>` | yes | with no usable row it recomputes once, before the staleness comparison, pinned to the PR head |
| `fno pr status <n>` | yes | reads through the same recompute-then-read helper |
| `finalize`'s auto-merge arm | not added, by decision | reached only from a terminal-allow, which implies `run_done` already ran this fire, and a failed arm leaves a green reviewed PR for a human |
| a human running the verb by hand | yes | `fno-agents review-coverage --cwd <dir> [--pr <n>] [--head <sha>]` |

The table is asserted, not trusted. `crates/fno-agents/tests/review_coverage_paths.rs` runs the Rust-drivable rows. It holds the safe-direction row to its reason. When a new call site reads `review_coverage` without joining the table, it fails. The verb's payload is pinned equal to `run_done`'s by `review_coverage_verb.rs`, on the whole data object.

The verb cannot assert coverage without performing the reads. There is no `--force`, no `--assume-covered`, and no skip key. A caller wanting a green gate must cause a review to exist. Its manifest-less defaults are strict: external review reads stay on, because `no_external` can only relax them. An unresolvable author session omits `self_attested_count` from the payload rather than reporting an unmeasured 0.

That pre-empts the aggregate-that-overstates-its-inputs shape at the field that becomes a bypass on the day anything enforces it.

### Zero rows vs a frozen streak: the discriminator

Two symptoms read alike and are different defects. Count `loop_check` rows in the worktree's own `.fno/events.jsonl`. Zero rows means the producer never ran there. A manual `fno-agents review-coverage --cwd <worktree>` settles it. Rows present with `consecutive_unchanged` frozen below `MUTE_PROBE_N` is the streak-gated shape the merge recompute makes moot.

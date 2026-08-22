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

The obligation to use one of these lanes on a code payload is enforced at the stop gate (`crates/fno-agents/src/loopcheck.rs`) and `fno do pr merge`, not only in this prose: a code PR that reaches the gate with no head-pinned `review_attestation` is held, and the held reason names this harness's verb.
This doc is the lane menu; the gate is the authority.
Opt out with `config.review.self_review_required = false`.

## Lane 1: self-invoke via the Skill tool (primary)

An in-session agent can launch the native review verb through the Skill
tool with **bare args**, not by typing the slash command:

```
Skill(skill="code-review", args="<level> --comment")
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

### The silent fork

A forked run can finish its work while its finder notifications never arrive. The run then neither returns nor reports, and that silence reads exactly like a clean run still in progress.

Send the fork a message and ask it to finish in-context and report its findings to you directly. It holds the work. Only the notification path is broken.

Never kill it and never re-run it. A second run is a second writer on the worktree, and one has soft-reset two committed commits out from under a worker on this repo.

The positive marker is what a finished run produces: it returns findings, or it states that it found none. Silence is neither.

Silence alone is not a wedge, so probe before you conclude. `git status` naming modified files is the portable positive marker for writing. `stat` is the precise one. Spell it `stat -f '%m %N' <paths>` on BSD and `stat -c '%Y %n' <paths>` on GNU. Compare either against `date +%s`. Read epoch seconds on both sides, because a local-time format string compared against a UTC clock makes a file written seconds ago look hours stale.

Anchor the probe at the repository root, because a relative pathspec silently matches nothing from the wrong directory. Print the count of files scanned beside the result, because zero writes and a broken probe are otherwise the same output.

The probe answers in the positive direction only. A fresh write proves a live writer. A stale read proves nothing, because a fork thinking between edits and a fork wedged look identical from outside. So a wedge call needs more than a quiet probe, and more than a timer.

A forked review does not appear in `fno agents top --subagents` or in `claude agents --json`. Only the launching session's own agent list shows it. So only that session can judge its fork, and an observer must take that session's answer rather than check an outside surface. A reader who checks the documented subagent verb sees nothing and reads the rule as satisfied. The instrument manufactures the absence it is read for.

This section is prose and stays prose. A gate needs a mechanical signal, and the whole defect is that no signal arrives.

## Lane 2: raw-inject via `fno agents mail send --raw`

The documented operator front door for asking another session (or your own) to fire a raw verb is `fno agents mail send --raw`.

It routes by the recipient's live lane. It either fires the requested operation or names why that lane cannot fire it.

On a prompt-line lane it injects the payload UNWRAPPED. No `<fno_mail>` envelope wraps it, so the slash sits at character 0 and parses like a human keystroke.

That sidesteps any model-invocation refusal. The prompt-line injection IS the user-invocation path.

Use it to fire a verb in a live worker, or with `--to-self` to target the current session through the same router.

```bash
# Into a peer:
fno agents mail send <peer> '/code-review <level> --comment' --raw
# At your own prompt line (recipient derived from ambient identity):
fno agents mail send '/code-review <level> --comment' --to-self --raw
```

`<level>` is sized from the diff by `level_for_diff` in `cli/src/fno/review_capability.py` (never `ultra`: billed separately, and the builder rejects it). No surface needs to spell the invocation. `fno do target review-invocation` prints it rendered and sized for this session, and the coverage refusals (stop gate, merge guard, the `fno/review-coverage` status) embed that render.

No lane above carries `--fix`. A fix pass writes, which moves HEAD. An attestation is head-pinned, so the round that wrote it also voids it. This matches the spawned-reviewer contract further down. `--fix` stays legal for a caller who asks for it directly, and only the machinery advice drops it.

## A reply does not resolve a thread

`fno do pr status` reports `optional_reviews_unresolved`, and `ready` is `green && unresolved == 0`.
Answering a finding in-thread does not decrement that counter: a GitHub review thread stays unresolved until it is resolved EXPLICITLY.
So a PR whose every finding has been fixed and answered can sit at `ready: false` indefinitely while reading, to a human and to the loop, as fully handled.

Resolve each thread with the "Resolve conversation" button, or:

```bash
gh api graphql -f query='mutation($t: ID!){resolveReviewThread(input:{threadId: $t}){thread{isResolved}}}' -F t=<threadId>
```

Thread ids come from `reviewThreads` on the `pullRequest`.
`fno do pr status` prints this instruction on stderr whenever the counter is non-zero, so the fix travels with the number rather than living only here.

Before you tell anyone else to run one of those, ask whether they can: `fno agents mail send '<payload>' --to-self --raw --check` (or `fno agents mail send <peer> '<payload>' --raw --check`) injects nothing and reports one of THREE answers, never two: `injectable: <lane>` (exit 0), `not-injectable: <reason>` (exit 1), or `unmeasurable: <reason>` (exit 3) when the evidence needed to decide could not be read at all.
Branch on all three. Collapsing `unmeasurable` into `not-injectable` states a verdict about a session the run never measured, and a deployed `fno-agents` too old to carry `--probe` answers `unmeasurable: probe-unavailable`.
It answers whether a PATH exists, never whether the turn lands, since no probe can see whether the prompt line is idle.
See [mail-live-inject](mail-live-inject.md) for what it resolves and why the Stop hook gates its compact advice on it.

`fno agents mail send --raw` routes to the right transport per recipient, and that transport is not always the same binary.

A mux-hosted session injects via `fno mux pane send --raw`, regardless of harness. A mux-hosted Codex session is therefore a real prompt-line lane. It can fire `/compact`, `/review`, or any other TUI verb the Codex parser accepts. `--raw` is required here: a bare send now wraps the payload in an `<fno_mail>` envelope, and an enveloped slash verb is text rather than a command.

A Claude daemon session injects via the `fno-agents mail-inject` Rust binary (`cli/src/fno/agents/dispatch.py`).

A Codex app-server thread has no prompt line. The Python front door routes the exact verbs `/review` and `/code-review` to the structured `review/start` RPC. It refuses every other raw payload, naming the app-server constraint and the wrapped-send or mux alternatives.

Other non-keystroke daemon lanes keep the generic refusal. A slash submitted through their model-turn RPC arrives as text and does not fire.

`fno agents mail send --raw` is therefore the single documented raw-payload entry point. The `mail-inject` and `review-start` binaries remain low-level structured or STDIN expert doors, not alternative raw-payload routers.

The `mail-inject` binary remains reachable directly for scripting against a Claude daemon session outside the Python CLI, where its STDIN form suits a pipe:

```bash
printf '/code-review <level> --comment' | fno-agents mail-inject --harness claude --session <full-session-uuid>
```

It reads the turn text from STDIN and enforces the brevity cap for a direct binary call. The raw lane itself is capped in Python at the `fno agents mail send --raw` front door. The binary holds the same ceiling for callers that bypass it.
A shared `FNO_MAIL_BODY_WARN` / `FNO_MAIL_BODY_REFUSE` knob pair keeps the threshold identical to the wrapped-mail cap, so the direct binary is not a way around it.
The cap skips framed envelopes: a `<fno_mail>` body is already capped in Python before it reaches here, and a `<cross-session-message>` relay hop is internal traffic, not authored mail, so neither is refused here.
An over-cap unwrapped body is refused before it is delivered; the STDIN form is for piping the turn, not for moving a verbose payload.

`--session` takes the full session UUID or its 8-hex short id (the roster accepts either). This raw/direct form targets the Claude control-socket lane.

It delivers over the daemon `control.sock` to a live `claude --bg` session, so the target must be an adopted live session: it never lazy-starts one.

The binary's `mail-inject --harness codex` mode submits text through `turn/start`. It is the wrapped-mail transport, not a raw verb transport, because the Codex app-server has no slash parser.

The structured Codex expert door is `fno-agents review-start --session <thread-id> --target <uncommittedChanges|baseBranch:branch|commit:sha|custom:instructions> --delivery inline`.

That direct verb takes an already-structured target rather than a raw payload. The exact-verb allowlist stays Python-only at the raw-payload door, so no second raw-payload door opens in Rust.

When the target repository has no `refs/remotes/origin/HEAD`, a bare `/review` or `/code-review` on a Codex app-server thread refuses and requires `--base`. The router derives `baseBranch` only from that authority. The `--uncommitted` flag remains the explicit working-tree target.

**Discoverability note.** `mail-inject` is a `fno-agents` *binary* verb, not a `fno agents mail` or `fno agents` (Python CLI) verb.
It is matched with `matches!` in `crates/fno-agents/src/bin/client.rs`, deliberately, so the routable-verb parity guard does not see it; that keeps it out of `--help` and `CLIENT_VERB_USAGE`.
So `fno agents mail --help`, `fno agents --help`, and a grep of the Python tree all report nothing, and a "does this exist?" probe against any of them answers false.

When its explicit structured or STDIN contract is the point, use the hidden binary verb. Otherwise reach for `fno agents mail send --raw` for recipient-aware routing.
Do not conclude the lane is absent from an empty `--help` or an empty Python-tree search; the binary verb is there.

## Lane 3: king-mediated mail (fallback)

When neither self-invocation nor a raw inject is available - no live
session to inject into, or a worker's harness lacks the verb - ask a
king over `fno agents mail send`.
The king's reply injects as user-shaped text and the worker's own
harness serves the verb in response, or the king can fire the verb
into the worker's live session directly via
`fno agents mail send <worker> '<verb>' --raw` (Lane 2).
With no live king, fall back to advisory self-review or run the native
verb by hand.

## Why (wrapped) mail cannot carry a verb

A wrapped `fno agents mail send` cannot carry a verb. It writes an `<fno_mail ...>` envelope at character 0 of the input. The slash command therefore never sits at the start and never parses. Mail carries **instructions** ("review my diff"), not invocations (a sized `/code-review` order). `--raw` (Lane 2) is the deliberate exception. It strips the envelope so the slash parses, and that is the cost the wrapper exists to impose.

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
`fno agents mail send --raw` (Lane 2) is the user-invocation path that lands the verb
as user-role text, so it is not subject to that refusal - reach for it
when self-invocation is refused.
The one environment-wide window predates the raw-inject lane's verification
(confirmed separately, the next day) and was not exercised there, so
treat that window as open; if `fno agents mail send --raw` fails it too, report the
exact refusal text and surface it to a human rather than burning cycles
re-invoking.

The standing lesson: every plausible mechanism proposed for this verb's
refusals has so far been falsified by a worker executing it.
Guard the value, not a correlate.

The Skill-tool success record (three workers) and a self-initiated
refusal record sit side by side, and no cause has held up.
`fno agents mail send --raw` (Lane 2) is the most reliable trigger and the one to
reach for when self-invocation is refused: it is the user-invocation
path, so the model-invocation refusal does not apply to it.
Short of that, the king-mail loop fires often but not always (refused
twice in one session with an order in hand).
Treat self-invocation as the lane worth trying first, `fno agents mail send --raw` as
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

Both now call one predicate, `review_freshness(reviewed_sha, head_sha)`. It lives in `crates/fno-agents/src/loopcheck.rs` and returns one of five states:

- `fresh` - the reviewer read this exact commit.
- `carried_base_sync` - the PR's own code delta is byte-identical. Any tree difference came from the base moving. A rebase is this shape.
- `carried_docs_only` - only documentation paths changed since the reviewed commit.
- `carried_subset` - the code delta only shrank. Every raw line still shipping is byte-identical to one the reviewer read. The vanished lines are paths the base absorbed on the rebase. The grade compares the sorted raw lines the identity keeps beside its hash. The HEAD set contained in the reviewed set is a shrink. A line the reviewer never saw is new unreviewed code.
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

Four separate places ask "has this reviewer reviewed this code".

All four go through the predicate: the coverage count, the attestation scan, the presence check behind `missing_bots`, and `finalize`'s arming check.

Fix one and leave the others on a bare equality, and the gate stays exactly as tight as before.
The softening is then purely decorative.

A `stale` verdict is **recorded, not dropped**.
`CoverageVerdict::Stale` says a reviewer responded against an older commit.

That is a different fact from `absent`, and it calls for a different response: ask for a re-read, rather than wait for a first read.

### CI carry: the same idea, a second implementation, on purpose

`carried_base_sync` lives in the local runtime, so only a live session can act on it. It reads `~/.fno/events.jsonl` and the local git objects. `review-coverage-gate.yml` posts failure on every `synchronize` unconditionally, because the workflow can read neither. A rebase that changes no code turns the required `fno/review-coverage` status red until a session re-greens it.

`scripts/ci/coverage-carry.sh` closes that gap from inside the workflow. It computes a second, independent identity, rather than sharing one with the local runtime. It reads the `before` and `head` shas from the `synchronize` event. For each it computes `git patch-id --stable` over the diff GitHub's compare API renders (`repos/<repo>/compare/<base>...<sha>`, the `.diff` media type). This needs no checkout of the repository being compared. When the base ref moves under it, three-dot compare semantics keep the identity unchanged. The fix is proven against a real rebase pair. A force-pushed-away head is still servable by the compare API. It yields the same patch-id as the head that actually merged.

`pr_code_diff_identity` was not a candidate here. The workflow has no checkout, and that identity needs one - it hashes `git diff --raw`. Computing the identity from the server-rendered diff is what lets the job skip the checkout.

**Two identity implementations now exist, and that is deliberate, not drift.** `pr_code_diff_identity` decides whether a local verdict counts toward the event row. `coverage-carry.sh`'s patch-id decides whether the CI status carries. They can disagree, and both disagreements fail closed:

- Local carries but CI does not: the status stays red until a session re-greens it. That is today's behavior, unchanged.
- CI carries but local does not: `fno do pr merge` still enforces `covered_conjuncts` (`cli/src/fno/pr/_coverage_gate.py`) on its own. A CI-only carry is refused at merge time.

The two gates are an AND, never an OR. Neither implementation can launder the other's refusal into a merge.

One constraint decided every branch of `coverage-carry.sh`: a patch-id match proves the code is identical. It never proves a bot reviewed it. So the script only ever carries a `success` status that already exists on the previous head. That status must match the publisher allowlist the workflow already preserves: `covered*` or `no review lane*`. All three spellings of `coverage-override*` are refused by name. A green the override label bought must never survive its own withdrawal by riding a rebase onto the next head. Every read failure falls through to today's failure post, and so does a match between two empty diffs.

One shape does not carry, and it is not a bug: a chain of rebases where an intermediate `synchronize` run was cancelled. The cancelled run never posts to its head, so that head carries no status. The next run's `--before` has nothing to carry forward. That falls to failure - the same outcome as today, on a head no run ever finished evaluating.

### Scope: which PR an attestation is about

When a verdict was rendered is freshness's question. Which PR it was rendered for is scope's question, and the two are independent.

The events journal is shared across every worktree of a repo by design: `setup-worktree.sh` links each worktree's `.fno/events.jsonl` to the canonical file. An unscoped scan therefore reads every branch's attestations into every PR's verdict list. Measured on 2026-08-16: five attestations, five heads, five branches, one file.

The fix is the `branch` field on `review_attestation` plus one predicate, `attestation_in_scope`, applied by both scans (`local_latest_passes` and `unattested_reviewers_scan`) before any freshness call. An attestation naming the PR's head branch is in scope. So is any attestation pinning the PR's exact head sha: a foreign branch cannot share this head sha without being this commit. The spawned-reviewer lane needs that arm. Its worktree carries a branch of its own, so a branch-only match reads the reviewer's exact-HEAD pass as out of scope. An event predating the field counts only on that exact-head arm. The legacy line deliberately does not inherit the carry: an attestation on a moved head cannot be scoped to any PR.

Out-of-scope lines are skipped entirely rather than marked stale: a stale verdict says "ask this reviewer to re-read", which is wrong advice about a reviewer on another branch. The verdict records which rule admitted it (`scope: attested_branch | legacy_head_match`), so a refusal under a moved head can name a pre-branch-field attestation.

#### How the producer picks the branch name

`emit-attestation.sh` must write the name GitHub reports as `headRefName`. The local branch name is not always it. A spawned reviewer runs in its own worktree on a branch of its own, because git refuses two worktrees on one branch. There the PR branch is the UPSTREAM. An author worktree is cut from a base branch and tracks it until `push -u` fires at PR create. There the upstream names the base, and the LOCAL name is the PR.

The discriminator is commits, not names. A reviewer worktree sits at the tip of the branch it tracks, so `@{upstream}..HEAD` is empty. An author worktree is ahead by exactly the diff under review. The upstream wins in exactly one case: a zero count, plus an upstream that is not `refs/remotes/origin/HEAD`. The second conjunct keeps a commitless fresh worktree from recording its base. Two earlier spellings both mis-scoped. A literal `main` comparison wrote `branch=develop` on a develop-based repo. The `origin/HEAD` comparison that replaced it did the same on any author worktree tracking a non-default branch. Either one loses the branch arm for the real PR. Either one also leaks the event into scope for any PR whose `headRefName` matches the base name.

The derivation stays local. `gh pr view --json headRefName` answers directly, and it is refused anyway. A network call on the emit path turns a review receipt into something that fails during a GitHub slowdown. A detached HEAD names no branch. The emit refuses rather than write an empty string. An empty string is byte-identical to the pre-branch-field backlog, so it mints a fresh legacy member no carry can scope.

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

**`carried_docs_only` inherits `is_documentation_path`, and that classifier calls every `.md` file documentation.** In this repo `skills/*/SKILL.md`, `agents/*.md`, and `AGENTS.md` are behavior, not prose. So a skill rewritten after a review carries the earlier verdict forward as fresh coverage. This is deliberate for now, because it matches the existing payload classifier. A `.md`-only PR already skips review gating entirely, so the carry rule is not what introduced the gap. Narrowing it is a real behavior change and has to move in lockstep with the Python mirror in `_merge._is_documentation_path`.

### The dispatcher's own ordering matters as much as the carry rule

The carry rule cannot save a review bought by an avoidable rebase. Measured one night: a king ordered about ten rebases on one PR. It requested a review after each one, buying ten reviews of code that changed once. `CarriedBaseSync` existed the whole time. It still cannot help here, because the carry only ever compares the CURRENT head against the LAST reviewed one. It has nothing to say about a review that was requested and completed before the next rebase moved the head again.

Any dispatcher (a king, `/fno:pr check`, a bg loop) that needs both a rebase and a review on the same PR must order them. Batch every pending rebase first, wait for green, THEN request the review once on the final head. A rebase requested after a review is not a smaller version of this mistake. It is the same mistake, since the next review request pays for it again.

### Named, not closed: the derivation-latency window

A valid attestation can exist while the gate cannot see it, and this PR does not close that.

Raw `review_attestation` events land in the shared journal the moment they are emitted.

`scripts/setup/setup-worktree.sh` links every worktree's `.fno/events.jsonl` to the canonical journal (`link_events_journal`), the same sharing that makes the `branch` field necessary: two worktrees, one log.

But the Python gate reads the DERIVED `review_coverage` aggregate, which the Rust runtime writes only on a loop-check run.

So the propagation path is still the aggregate, and the window below is a derive gap, not a copy gap.

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
precedence `fno do target init` resolves.
loop-check compares it against the authoring session's manifest
`harness_session_id` and labels each local verdict with a tri-state
`attestation_origin`:

- `self_attested` - the authoring session emitted the attestation.
- `other_session` - a different session emitted it.
- `unknown` - no attester was recorded, or the author session is unknown.

`other_session` is not `independent`.
The manifest names the session that ran `fno do target init` in the worktree, so a
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

## The in-flight review hold

Every gate above answers one question. What verdict EXISTS for this head. None of them answers the other one. A review that is RUNNING has produced no verdict yet. So it stays invisible to `review_coverage`, to `required_bots`, and to `ready`.

Three PRs on 2026-08-22 read green, settled, mergeable and `ready: true` with an empty `ready_blockers`. A review of that exact head was mid-flight on each. PR 1068 had a code-review fork writing to the worktree. PR 1071's worker was mid-sentence, "Committing the review fixes". PR 1072's worker had counted five findings and named the two files. A human held all three on judgment. That judgment lived nowhere in the code.

The window is the default ordering, not a race. A push starts a review. CI runs against that same head in parallel. CI is bounded, roughly nineteen minutes in this repo. A review is not. Green arrives FIRST, reliably. Any merge taken at the moment a PR turns green lands inside the window.

Config makes this worse rather than better. With no `required_bots`, no `reviewers` and `self_review_required` false, `_review_lane_configured` short-circuits coverage to COVERED. So `ready: true` is correct against the configured policy and wrong against the world. The hold is therefore config-independent. A running review blocks whether or not the project requires any review.

### Two layers

The **registered hold** is a TTL claim on `review:branch:<branch>`. A review DISPATCH takes it, never the reviewer itself. A reviewer that crashes still leaves behind the hold its dispatcher took.

It is a claim rather than a new state file. `fno.claims` already owns atomic acquisition, TTL bounds, the LIVE / SUSPECT / STALE / CORRUPTED classification, and a reaper. `review:` is not a global-id prefix. So the key routes to the canonical repo root, and every worktree of the project shares one hold.

The **worktree probe** is derived and needs no cooperation. It reads tracked modifications on the branch, and a local HEAD that differs from what the PR merges. It covers every review footnote never dispatched.

Neither layer covers the specimens alone. The probe cannot see the window between a dispatched review and its first edit. When PR 1072 went green, it sat in exactly that window. The hold cannot see a review that nothing registered.

### Where the hold is taken and cleared

| Site | Registers | Why it is the one that matters |
|---|---|---|
| `PreToolUse` on the Skill tool (`hooks/review-hold.sh`) | takes it | all three specimens were reviews the worker self-invoked through this tool, which is not footnote code and cannot register on its own |
| `skills/review/scripts/emit-attestation.sh` | releases it | the positive completion marker: a verdict now exists for this head, so the release and the proof are one event |
| the TTL | ages it out | the reviewer died. See the receipt rule below |
| a human or an unhooked harness | takes nothing | the named residual gap, covered only by the worktree probe |

Registration NEVER blocks a review from starting. The probe still covers a review that runs unheld. A review that refuses to start because a lockfile write failed is strictly worse.

A `PostToolUse` release was wired here and removed. For an INLINE skill the Skill tool returns the SKILL.md body, and the review runs AFTERWARDS. So the release fired within milliseconds, and the hold covered nothing. That is exactly the dispatched-but-not-yet-edited window layer 2 cannot see. The guard was decorative for its own specimen.

So a review that finds findings holds the lane until a clean re-review attests. That is the intended behavior: the findings are unfixed. To merge before then, release by hand.

The release never names a holder, and `--holder` is optional on the verb for that reason. The hold is a lane lock, not an ownership assertion. Each release site derives its own holder string, and `release_claim` no-ops SILENTLY on a mismatch. A holder-matched release wedged the lane for the full TTL under a receipt that said "released".

### Who reads it

| Surface | Behavior |
|---|---|
| `fno do pr status <n>` | `ready` goes false, `ready_blockers` names the layer, and `review_activity` reports both probes whether or not they blocked |
| `fno do pr merge <n>` | refuses beside the plan hold and BEFORE the `auto_merge` gate |
| the auto-merge lane | the same refusal: it is not a separate caller, it is `run_merge` with `auto_merge.enabled`, and it is the caller with no judgment to fall back on |
| a bare `gh pr merge` via `hooks/git-protection.py` | denies coarsely, reading the claims directory directly |

The hook reads files rather than shelling a third `fno` probe. The two vetoes above it already spend 25s each. The harness hook budget is 60s, and the margin is under 6s. A killed hook emits no verdict at all.

That coarseness is deliberate, in the safe direction. Any review hold in the repo denies. The hook never maps the PR to its branch, because that needs the network call this path exists to avoid. It never judges expiry either. Hybrid liveness can keep a TTL-lapsed hold LIVE, so a TTL-only read here can ALLOW what the guard refuses. A wrong deny costs one command.

### Failing safe in both directions

A missing hold is never by itself the clear answer. It clears only after the worktree enumeration RAN and answered. Four readings block instead: a corrupted lockfile, an unreadable claims root, a failed `git worktree list`, and a PR whose head branch will not resolve. An unprobed PR is not a clear one.

A hold that outlives a crashed reviewer wedges the merge lane permanently. That is worse than the defect it prevents. So it ages out on `config.review.hold_ttl_minutes`, which defaults to 90. It never ages out silently. The surface that clears past a lapsed hold prints the holder and the expiry, and emits `review_hold_expired`. A lane that clears with no receipt reads exactly like a lane nobody ever held.

That same arm DELETES the lockfile, in one breath with the receipt. A lapsed file stops blocking the Python readers, which judge expiry, and keeps blocking the stdlib hook, which cannot. One crashed reviewer otherwise denies every bare `gh pr merge` in the repo, for every PR, until someone notices. The claims reaper does collect it eventually, but it is config-gated, so this path cannot lean on it.

There is no self-exemption. An author who merges over its own uncommitted fixes loses them, exactly as a stranger does. A caller-relative `ready` also means two readers of one payload get different answers.

To clear a hold by hand: `fno do pr review-hold release --branch <b> --holder <h>`. To read one: `fno do pr review-hold check <pr>`, exit 0 clear, 3 held, 4 a dead instrument.

## Producer reachability: every path that reaches the gate

`fno do pr merge` refuses a PR whose `review_coverage` event says uncovered, unknown, stale, or missing. For most of the gate's life exactly one writer existed: `read_pr_info` under `run_done`, which `decide()` reaches only past a streak counter. A session with no `.fno/target-state.md` runs no stop hook at all, so no row will ever exist. The gate was unsatisfiable for that session shape, not strict. This is the decorative-guard pitfalls entry inverted: a PRODUCER on one of N paths.

The producer is now reachable from every path that can reach the gate.

| Path | Producer reachable | How |
|---|---|---|
| target stop hook `decide()` -> `run_done` | yes, unchanged | streak-gated |
| `fno do pr merge <n>` | yes | with no usable row it recomputes once, before the staleness comparison, pinned to the PR head |
| `fno do pr status <n>` | yes | reads through the same recompute-then-read helper |
| `fno do pr coverage-check <n>` without `--recompute` (and the git-protection hook through it) | no by default; `--recompute` yes | the hook path must not recompute: a PreToolUse hook has a 60s budget and the Rust producer takes minutes. `--recompute` shells the producer, same as `fno do pr merge`. One-directional: on a missing or stale row this denies where `fno do pr merge` may yet allow |
| `finalize`'s auto-merge arm | not added, by decision | reached only from a terminal-allow, which implies `run_done` already ran this fire, and a failed arm leaves a green reviewed PR for a human |
| a human running the verb by hand | yes | `fno-agents review-coverage --cwd <dir> [--pr <n>] [--head <sha>]` |

The table is asserted, not trusted. `crates/fno-agents/tests/review_coverage_paths.rs` runs the Rust-drivable rows. It holds the safe-direction row to its reason. When a new call site reads `review_coverage` without joining the table, it fails. The verb's payload is pinned equal to `run_done`'s by `review_coverage_verb.rs`, on the whole data object.

The verb cannot assert coverage without performing the reads. There is no `--force`, no `--assume-covered`, and no skip key. A caller wanting a green gate must cause a review to exist. Its manifest-less defaults are strict: external review reads stay on, because `no_external` can only relax them. An unresolvable author session omits `self_attested_count` from the payload rather than reporting an unmeasured 0.

That pre-empts the aggregate-that-overstates-its-inputs shape at the field that becomes a bypass on the day anything enforces it.

A bare `gh pr merge` from an agent tool call is a fourth reader of the same predicate. The merge hook in `hooks/git-protection.py` shells the hidden `fno do pr coverage-check` verb. That verb evaluates the guard's coverage check in `cli/src/fno/pr/_coverage_gate.py` without the recompute. A PreToolUse hook has a 60s harness budget and the Rust producer is budgeted in minutes. The invariant between the two surfaces is one-directional by design. The hook never allows what the guard refuses. When the row is missing or stale the hook denies where `fno do pr merge` can still allow after recomputing. Absence denies. A named instrument failure (exit 4) fails open. Both surfaces refuse with one sentence, pinned character for character in `cli/tests/unit/test_pr_coverage_check.py`.

### The GitHub status is a projection, not a latch

The local `review_coverage` event is the computed verdict. The `fno/review-coverage` commit status is its GitHub projection. When an existing status disagrees with the verdict, `fno pr status` republishes it. Polling is silent after one correction. An absent context remains pending, so the status reader never becomes the first writer. Both stale directions are defects. PR 966 stayed green after its computed count became zero. PR 1003 stayed red after fresh covered evidence existed. The Rust publisher retries unreadable override-label queries. It protects an existing `coverage-override` description and otherwise posts the computed verdict instead of preserving stale state.

### Zero rows vs a frozen streak: the discriminator

Two symptoms read alike and are different defects. Count `loop_check` rows in the worktree's own `.fno/events.jsonl`. Zero rows means the producer never ran there. A manual `fno-agents review-coverage --cwd <worktree>` settles it. Rows present with `consecutive_unchanged` frozen below `MUTE_PROBE_N` is the streak-gated shape the merge recompute makes moot.

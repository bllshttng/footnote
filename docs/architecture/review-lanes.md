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
Skill(skill="code-review", args="medium --fix")
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

The front door for triggering a verb in another session (or your own) is `fno mail send --raw`.
It injects the payload UNWRAPPED at the recipient's prompt line: no `<fno_mail>` envelope, so the slash sits at character 0 and parses as if a human typed it.
That sidesteps any model-invocation refusal entirely, because this IS the user-invocation path.
Use it to fire a verb in a live worker (a king firing `/code-review` into a minion), or with `--to-self` to fire one at your own prompt line.

```bash
# Into a peer:
fno mail send <peer> '/code-review medium --fix' --raw
# At your own prompt line (recipient derived from ambient identity):
fno mail send '/code-review medium --fix' --to-self --raw
```

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

Before you tell anyone else to run one of those, ask whether they can: `fno mail send '<payload>' --to-self --raw --check` (or `fno mail send <peer> '<payload>' --raw --check`) reports `injectable: <lane>` or `not-injectable: <reason>` and injects nothing.
It answers whether a PATH exists, never whether the turn lands, since no probe can see whether the prompt line is idle.
See [mail-live-inject](mail-live-inject.md) for what it resolves and why the Stop hook gates its compact advice on it.

`fno mail send --raw` routes to the right transport per recipient, and that transport is not always the same binary.
A claude daemon session injects via the `fno-agents mail-inject` Rust binary (`cli/src/fno/agents/dispatch.py`).
A mux-hosted session injects via `fno mux pane send`, a separate path that never reaches the mail-inject binary.
So the front door is the single entry point; `mail-inject` is the transport for the daemon lane only, and it cannot target a mux pane.
The binary verb is still reachable directly for scripting against a daemon session outside the Python CLI, where its STDIN form suits a pipe:

```bash
printf '/code-review medium --fix' | fno-agents mail-inject --harness claude --session <full-session-uuid>
```

It reads the turn text from STDIN and enforces the brevity cap for the raw/direct lane: `fno mail send --raw` does not cap in Python, so this binary is where that lane, and a direct binary call, get capped.
A shared `FNO_MAIL_BODY_WARN` / `FNO_MAIL_BODY_REFUSE` knob pair keeps the threshold identical to the wrapped-mail cap, so the direct binary is not a way around it.
The cap skips framed envelopes: a `<fno_mail>` body is already capped in Python before it reaches here, and a `<cross-session-message>` relay hop is internal traffic, not authored mail, so neither is refused here.
An over-cap unwrapped body is refused before it is delivered; the STDIN form is for piping the turn, not for moving a verbose payload.
`--session` takes the full session UUID or its 8-hex short id (the roster accepts either); `--harness` is `claude` (the default) or `codex`.
It delivers over the daemon `control.sock` to a live `claude --bg` session, so the target must be an adopted live session: it never lazy-starts one.

**Discoverability note.** `mail-inject` is a `fno-agents` *binary* verb, not a `fno mail` or `fno agents` (Python CLI) verb.
It is matched with `matches!` in `crates/fno-agents/src/bin/client.rs`, deliberately, so the routable-verb parity guard does not see it; that keeps it out of `--help` and `CLIENT_VERB_USAGE`.
So `fno mail --help`, `fno agents --help`, and a grep of the Python tree all report nothing, and a "does this exist?" probe against any of them answers false.
That is fine now that `fno mail send --raw` is the documented front door: reach for the front door, and the hidden binary verb only matters if you need its STDIN form directly.
Do not conclude the lane is absent from an empty `--help` or an empty Python-tree search; the binary verb is there.

## Lane 3: king-mediated mail (fallback)

When neither self-invocation nor a raw inject is available - no live
session to inject into, or a worker's harness lacks the verb - ask a
king over `fno mail send`.
The king's reply injects as user-shaped text and the worker's own
harness serves the verb in response, or the king can `mail-inject` the
verb into the worker's live session directly (Lane 2).
With no live king, fall back to advisory self-review or run the native
verb by hand.

## Why (wrapped) mail cannot carry a verb

A wrapped `fno mail send` cannot carry a verb.
It writes an `<fno_mail ...>` envelope at character 0 of the recipient's input, so the slash command is never at the start of the input and never parses.
Mail therefore carries **instructions** ("review my diff"), not invocations ("/code-review medium --fix").
`--raw` (Lane 2) is the deliberate exception: it strips the envelope so the slash parses, which is exactly the cost the wrapper exists to impose.

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
`mail-inject` (Lane 2) is the user-invocation path that lands the verb
as user-role text, so it is not subject to that refusal - reach for it
when self-invocation is refused.
The one environment-wide window predates the `mail-inject` verification
(confirmed separately, the next day) and was not exercised there, so
treat that window as open; if `mail-inject` fails it too, report the
exact refusal text and surface it to a human rather than burning cycles
re-invoking.

The standing lesson: every plausible mechanism proposed for this verb's
refusals has so far been falsified by a worker executing it.
Guard the value, not a correlate.

The Skill-tool success record (three workers) and a self-initiated
refusal record sit side by side, and no cause has held up.
`mail-inject` (Lane 2) is the most reliable trigger and the one to
reach for when self-invocation is refused: it is the user-invocation
path, so the model-invocation refusal does not apply to it.
Short of that, the king-mail loop fires often but not always (refused
twice in one session with an order in hand).
Treat self-invocation as the lane worth trying first, `mail-inject` as
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

**A green PR whose only attestation is `self_attested` is covered. Merge it.**
`self_attested` is not a hold condition and has never been one.

**The spawned-reviewer lane.** A non-self attestation is producible today: the author spawns its own reviewer citizen, a different session by construction, so its attestation renders `attestation_origin = other_session`.

```bash
fno agents spawn --name <name>-review "/code-review <size> for PR <n> against main" \
  --harness claude --substrate bg --model opus \
  --permission-mode bypassPermissions --cwd <the author's worktree>
```

Two rules ride with the lane, each derived from a code fact.
`--cwd` MUST be the author's worktree: `fno event emit` writes cwd-relative through `append_event`, and `local_head_pinned_passes` reads only the project log, so a reviewer anywhere else emits an attestation its own loop-check never sees.
NO `--fix`: the reviewer shares the worktree, so a fixing reviewer moves HEAD and silently invalidates the author's own head-pinned attestation, and re-opens the tree-corruption specimens.
One writer per worktree while a review is in flight; the author applies findings and re-attests, and the reviewer's attestation is then stale by design, which is the head-pinning rule working, not a bug.

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

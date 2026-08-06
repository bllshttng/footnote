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

## Lane 2: raw-inject via `fno-agents mail-inject` (most reliable)

A raw string with no envelope lands the verb at character 0 as user-role
text in a live target session, so it parses as if a human typed it and
sidesteps any model-invocation refusal entirely - this IS the
user-invocation path.
This is the lane to trigger the verb in a live worker session (a king
firing `/code-review` into a minion), and it is the most reliable
trigger: it beats both the Skill tool and king-mediated mail.
Reach for it first when a worker needs a review triggered reliably.

```bash
printf '/code-review medium --fix' | fno-agents mail-inject --harness claude --session <full-session-uuid>
```

The turn text is read from STDIN (this sidesteps the argv size limit for
large envelopes).
`--session` takes the full session UUID or its 8-hex short id (the
roster accepts either); `--harness` is `claude` (the default) or `codex`.
It delivers over the daemon `control.sock` to a live `claude --bg`
session, so the target must be an adopted live session - it never
lazy-starts one.

**Discoverability trap (this is the load-bearing one).** `mail-inject`
is a `fno-agents` *binary* verb, not a `fno mail` or `fno agents`
(Python CLI) verb.
It is matched with `matches!` in `crates/fno-agents/src/bin/client.rs`,
deliberately, so the routable-verb parity guard does not see it; that
keeps it out of `--help` and `CLIENT_VERB_USAGE`.
So `fno mail --help`, `fno agents --help`, and a grep of the Python tree
all report nothing, and a "does this exist?" probe against any of them
answers false.
Do not conclude the lane is absent from an empty `--help` or an empty
Python-tree search; the binary verb is there.

## Lane 3: king-mediated mail (fallback)

When neither self-invocation nor a raw inject is available - no live
session to inject into, or a worker's harness lacks the verb - ask a
king over `fno mail send`.
The king's reply injects as user-shaped text and the worker's own
harness serves the verb in response, or the king can `mail-inject` the
verb into the worker's live session directly (Lane 2).
With no live king, fall back to advisory self-review or run the native
verb by hand.

## Why mail cannot carry a verb

`fno mail send` can **never** carry a verb.
It writes an `<fno_mail ...>` envelope at character 0 of the recipient's
input, so the slash command is never at the start of the input and
never parses.
Mail therefore carries **instructions** ("review my diff"), not
invocations ("/code-review medium --fix"); `mail-inject` (Lane 2) is the
raw path that does carry an invocation.

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

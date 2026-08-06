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

## Lane 2: king-mediated mail (fallback)

When self-invocation is refused (see below) or a worker's harness lacks
the verb, ask a king over `fno mail send`.
The king's reply injects as user-shaped text and the worker's own
harness serves the verb in response.
With no live king, fall back to advisory self-review or run the native
verb by hand.

## The constraint that shapes both lanes

`fno mail send` can **never** carry a verb.
It writes an `<fno_mail ...>` envelope at character 0 of the recipient's
input, so the slash command is never at the start of the input and
never parses.
Mail therefore carries **instructions** ("review my diff"), not
invocations ("/code-review medium --fix").
The contrast is a raw-inject lane, which delivers a string with no
envelope so a verb lands at character 0 as user-role text and parses as
if a human typed it.
A raw-inject lane would be the most reliable trigger because it sidesteps
any model-invocation refusal entirely (it is the user-invocation path),
but this repo's `fno mail` and `fno agents` surfaces do not currently
expose one, so it is not a lane available here.

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
In such a window the only confirmed path is the one the error itself
names: a human typing `/code-review`.
If you hit this, do not burn cycles re-invoking or spawning peers;
report the exact refusal text and stop, then surface it to a human and
wait.

The standing lesson: every plausible mechanism proposed for this verb's
refusals has so far been falsified by a worker executing it.
Guard the value, not a correlate.

The Skill-tool success record (three workers) and a self-initiated
refusal record (a different exchange saw a session's own review
attempts refused while mailed orders fired) sit side by side.
Neither is the whole picture: self-invocation has worked and has been
refused, mailed orders are the more reliable trigger but are also not
guaranteed (refused twice in one session with an order in hand), and no
cause has held up.
Treat self-invocation as the lane worth trying first and mail as the
fallback, not as a closed either/or.
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

# Output style and skill precedence

The condensed form lives in `AGENTS.md` so it reaches every harness at session start. This file holds the full ruleset, kept out of the auto-loaded preamble to stay under the byte budget. Load it when the condensed form is not enough.

## Precedence over generic coding skills

A generic coding skill (ponytail, karpathy-guidelines, and anything similar installed globally) is advisory here. Where it disagrees with the working principles in `AGENTS.md`, **that file wins.** The skills are installed per-machine and know nothing about this repo; those principles were written on purpose.

Two conflicts are live today and both resolve the same way:

- **"Shortest diff wins" loses to "fix what you find."** Ponytail optimises for the smallest change and fewest files. Principle 4 requires a pre-existing problem discovered mid-task to be fixed in the same PR while context is warm, as its own atomic commit. Fix it.
- **No tool-branded comments.** A `// ponytail:` marker is a comment written for a tool rather than for the next reader, which is what the comment principle rules out. Explain the invariant or the why-not-the-obvious in plain terms, or write nothing.

What the skills get right and `AGENTS.md` already says: minimum code that solves the problem, reuse what the repo already has, no speculative abstractions. That overlap is not a conflict, and it is stated in that file in this repo's own terms.

## Output style

The reader has ADHD. Shape every response so it can be acted on:

1. Lead with the answer or next action: command, path, or snippet first.
2. Number multi-step work; one bounded action per step.
3. End with one next action doable in under two minutes.
4. Finish the current issue before raising a new one.
5. Restate progress each turn ("step 3 of 5 done").
6. Give time estimates in concrete units, never "a bit".
7. After a change, show what now works.
8. Errors: state location, cause, and fix. No drama.
9. Cap lists at 5 items.
10. No preamble, no recaps, no closers.

Exceptions: explain fully when asked to explain. Confirm before destructive actions. After three failed fixes, stop and name the doubtful assumption. If the request is ambiguous, ask one short question.

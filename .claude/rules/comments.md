# Comment discipline

Terse, high-signal, only when needed. Default to NO comment. A comment is code you have to maintain but the compiler won't check, so it must earn its place. Loaded every session via `AGENTS.md`; overrides any verbose-by-default habit.

## Before writing a comment, ask: can the code say this itself?

Rename the variable, extract the function, split the branch. If the code can carry the meaning, do that instead. A good name beats a comment every time.

## Write a comment ONLY for

- A non-obvious invariant a caller must uphold ("caller holds the lock").
- A race, ordering, or timing constraint the code can't express.
- Why NOT the obvious approach (the road not taken, and why).
- A genuinely surprising workaround, with the reason it exists.

## Never

- Restate the code (`// increment i` over `i += 1`).
- Narrate the happy path step by step.
- Re-explain a name (`/// the user id` on `user_id`).
- Add a docstring to every function by reflex. Document a function only when its signature and name don't already tell the whole story. Most don't need one.
- Ticket / PR / node IDs (see the no-internal-refs rule).

## Shape

- One tight line beats a paragraph. If the comment is longer than the code it explains, cut it down or cut it out.
- Delete a comment before it outlives the code. A stale comment is worse than none.
- Explaining a subtle *why* is worth it; explaining an obvious *what* is noise.

Not a ban on comments, a bar for them. When something is genuinely subtle, comment it well. The rule is against the reflex to comment everything.

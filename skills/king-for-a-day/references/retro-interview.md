# The retro-interview prompt (canonical)

The template a Director runs as a **standard post-epic court step** - epic complete (every wave merged), before final abdication. Load it from [Post-epic: interview the court](../SKILL.md#post-epic-interview-the-court).

This is the ceremony the x-304c synthesis marked `ADD`: the retro interviews were the best-performing ritual of that evening, yet they were not a ceremony at all - the maintainer hand-asked the Director to interview each builder and had to prod with follow-ups carrying the dogfooding lens. The lens is now baked in, so the interview fires without human prodding. The output turns one epic's chaos into filed, mechanism-bearing graph items instead of folklore.

## When it fires and who it targets

- **When:** the epic's last wave has merged and you are about to abdicate. One pass, one interview per builder, then exit.
- **Who:** every builder that carried a node in this epic. By the time the epic is done the builders have shipped and released their node claims, so `fno backlog epic status` (which derives `worker` from a live claim) and `top` / `discovered-json` (live sessions only) no longer map node -> builder - they show only who is still up. The durable map is the **ledger** (`fno.paths.ledger_json()`, `~/.fno/ledger.json` by default but honor a `config.paths.ledger_json` / `config.state_dir` override), which records `node -> run -> pr` per shipped node; read it to enumerate the epic's builders. Reaching a specific builder to interview it is then best-effort: mail the still-reachable ones (resolve a live handle from `fno agents discovered-json` / `top`), and for a builder gone to transcript-and-artifacts only, reconstruct its account from its transcript or note it unreachable. Do not treat the ledger's run id as a resumable session handle - it is the target run id, not the provider session id `fno agents resume` takes.
- The Director interviews because it holds cross-session knowledge a builder lacks - it saw every squad, so it can ask the sibling-aware follow-up a siloed builder cannot self-generate.

## Delivery mechanics

The interview is mailed. Follow the [minion delivery doctrine](minion-clause.md) in full, **including its peek-before-resend guard**: `fno mail send <builder-handle> "<prompt>" --from-self`. Both `delivered (hosted)` and `delivered (woken)` (an asleep-but-resumable builder woken to receive it) are success; any other receipt is undelivered, so peek the handle then re-send rather than trusting the queue (a `queued (durable)` interview is one the builder may never see). Resolve a live `<builder-handle>` from `fno agents discovered-json` / `top`. `fno mail send` wraps the body in its own `<fno_mail>` envelope, so the prompt below is body-only - do not add a second envelope.

## The prompt (dogfooding lens baked in)

Paste this as the `fno mail send` body (it is body-only; `fno mail send` supplies the `<fno_mail>` envelope), filling the epic/node slots and the session-specific block:

```
Post-epic retro for <epic> (your node: <node>). You are one of several builders being interviewed; your first-person account becomes a filed project artifact, so be concrete and name ids/paths/commands.

Lens: footnote building footnote. Answer as the fresh agent you were when you jumped in cold.

1. GAPS - what did you need that was not there, or was there but wrong? Name the verb, file, or receipt.
2. ROUGH EDGES needing polish - what worked but cost you time, retries, or a workaround? Include anything you had to rediscover that should have been handed to you.
3. WHAT WORKED vs WHAT DID NOT - which contracts/gates/verbs earned their place, and which fought you? Be specific about which, not "the tooling".
4. CEREMONY audit - which steps felt unnecessary, confusing, or inverted for the size of your change? Which would you delete, and which would you keep even though they slowed you?
5. <session-specific questions - the king fills these from what it saw this builder hit: a claim flap, a stale base, a review it drained, a fork it survived. One or two, concrete. Omit the line if none.>

Reply by mail (--from-self). I will ask one follow-up per thin answer.
```

## The dig-deeper rule (baked in, no human prod)

The maintainer's manual step was prodding thin answers with the lens. Do it yourself: for any answer that is a bare verdict ("the receipt was wrong", "ceremony felt heavy") with no id, no command, and no consequence, send **one** follow-up naming the missing anchor - "which line of the receipt, and what did the real state turn out to be?" One follow-up per thin answer, not an interrogation; the goal is a mechanism, not a transcript.

## Where the account lands

Write each builder's account as `<date>-<session-short>-<node>.md`, with frontmatter pinning `node`, `session`, `prs`, `epic`, `date`. footnote has no dedicated retro-path config, so retros follow the plan artifact store deterministically. `fno plan path --slug <x>` returns a file path of the form `<plans-dir>/<name>.md`; take its directory (`dirname`) to get the configured plans dir (the maintainer instance is the vault's `internal/fno/plans/`), and write retros to that dir's `retros/` sibling - `../retros/` relative to the plans dir. Resolve it that way rather than hard-coding a path - `internal/` itself is a gitignored vault symlink absent on a fresh checkout, and the plan-path resolver is what honors each project's config. These accounts are the sources a later synthesis pass folds (the x-304c synthesis pinned six such files in its `sources:` frontmatter). The interview produces the raw accounts; synthesis is a separate pass.

## Retro epistemics (how much to trust what comes back)

A retro is three kinds of claim, and they are not equally reliable (doctrine #5 from the x-304c synthesis):

- **Substrate-checkable facts** (a receipt said X, a claim read Y) - trustworthy *after* you check them against the graph / transcript / `gh`, not before. A builder can misremember an id.
- **Experience reports** (this fought me, that felt clean) - primary UX data, the thing the interview exists to capture. They are real signal about friction even when the builder misattributes the cause.
- **Self-assessments** ("I should have done X") - counterfactual wishes, not findings. Note them, do not file them as fixes.

Retros generate hypotheses; evals with ground-truth joins confirm them; and the orchestrator/peer view covers what a silo cannot see - a builder cannot report friction it never detected. The instrument is that triangle, not any single account. Do not promote a single-account wish to a graph node from the interview alone; that is the synthesis pass's job, under its two-plus-sessions bar.

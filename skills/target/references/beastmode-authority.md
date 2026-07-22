# Authority: the `beastmode` grant

Read this when the run carries `authority: full` (invoked as `/target beastmode`, surfaced on the `attended` line of `fno target status --json`). It governs judgment calls that would otherwise emit `<help>` and stall.

`/target beastmode "..."` grants **walk-away authority** for the session.
It composes with every other modifier, so `/target beastmode auto-merge "..."` is true overnight mode.

**What it changes: judgment, never irreversibles.**
An overnight walk must not stall at 2am on a call you would have made in five seconds at 9am.
So under authority, a judgment call is *decided and recorded* instead of stopping the session.
It grants no new powers: merge stays on the auto-merge axis, and destructive or credential-blocked work still stops.

**Why this is not called `yolo`.** `--yolo` is already taken, codebase-wide: it sits in the CLI's GLOBAL short-flag register (`-Y --yolo`) whose whole contract is that a global short "means the same thing on every command and never carries a per-command meaning".
Its one meaning is the provider dangerous-mode bypass (`fno agents spawn --yolo` -> codex `--dangerously-bypass-approvals-and-sandbox`, claude `bypassPermissions`).
A judgment grant is a different axis entirely, so it gets a different word and deliberately carries **no short flag** at all.
This matters most when spawning: in `/agent spawn /target <node> beastmode yolo`, `beastmode` binds to the target session's judgment and `yolo` to the spawn's permissions, with no token doing double duty.
A beastmode session still asks the harness for permission exactly as before, and a yolo-spawned worker still stops on an architecture fork unless it also holds an authority grant.

**Spellings.** `beastmode`, `beast`, and `beast mode` all mean the same modifier, case-insensitively.
The two-word form exists because mobile autocorrect splits `beastmode`, and it is the dangerous one: the stray `mode` token must be stripped along with the modifier.
Whatever the spelling, pass ONLY the bare node id to `fno target start --beastmode <node>` - init's node guard is anchored, so any leftover token means no node, therefore no claim, therefore a refused grant.
The CLI accepts `--beastmode` and `--beast`.

**The grant needs a BACKLOG NODE; free text cannot hold it, and neither can an unlinked plan.**
Authority is anchored to the session's claim, and init claims only `node:<id>` - resolved from a node input, or from a plan that resolves to a node in the graph.
A free-text run claims nothing; so does a standalone plan file that no node points at.
In both cases there is no anchor at all: `owner_pid` does not count (it is a transient init subprocess, and its liveness says nothing about whether the grant will outlive this moment), so nothing could distinguish that session from one that crashed and left its manifest behind.
Rather than let a grant outlive its session, an unanchored one is refused and `fno target init` says so at the point it happens.
Bind a node first (`/think` then `/blueprint` files one), then run `/target beastmode <node>`.

**How to read the grant.** Pass `--beastmode` to `fno target start` / `fno target init`; init stamps `authority: full` into the manifest (the field is absent otherwise).
Read it back from `fno target status --json` - the `attended` line carries `; authority: full (beastmode)` when the grant is live.
Read that line, NOT the raw manifest, and never the bare `authority:` field: authority **fails closed**, requiring a live claim where the `attended` verdict merely biases toward live.
A live `owner_pid` is deliberately NOT enough - it is alive for every session at init time, so it cannot tell a durable grant from one about to lapse.
That asymmetry is deliberate - a wrongly-live `attended` costs you one unnecessary prompt, while a wrongly-live authority grant silently un-prompts every future session that reads it (x-4af4: a defunct manifest once auto-locked an attended `/think` for ten days).

**Under `authority: full`, the deviation rules become:**

| Situation | Without authority | With `authority: full` |
|---|---|---|
| Bug in plan | fix inline, note it | unchanged |
| Minor enhancement (<15 min) | implement, note it | unchanged |
| Architecture decision, missing dependency, ambiguous requirement | STOP, emit `<help>` | **decide, record one ledger entry, continue** |
| Interactive prompt (`AskUserQuestion`) in a composed skill | ask the operator | **take the recommended option, record it** |
| Missing credentials, destructive ambiguity, a genuine blocker | STOP, emit `<help>` | unchanged - still stops |

The split is what the session can *undo*. A wrong architecture call costs a review comment; a wrong destructive call costs data.
When a decision is close, prefer the reversible option and say so in the entry.

**Which existing `<help>` sites flip: none.**
Every `<help>` currently written into this skill and its references is already a genuine blocker rather than a judgment call - `handoff-claim-lost` and `handoff-restore-failed` (another worker may own the node), `handoff-chain-exhausted`, `required-bot-quota-exhausted` (an external provider is out of quota), `pr-node-link-failed` (a write that did not stick), `restart-recommended` (minting and superseding graph nodes is not reversible), and `cross-project-disambiguation` (which skips one message rather than stopping the session).
Authority changes none of them.
It governs the decisions you would otherwise stop and ask about *without* a written `<help>` site - the architecture forks, the ambiguous requirements, the interactive prompts inside composed skills - which is exactly where an overnight walk actually stalls.

**The Autonomous Decisions ledger.** Nothing enforces this section - no code reads `authority` or writes an entry, so the ledger is a behavioral contract you keep, exactly like the decide-and-record rule it records.
That is deliberate (the grant changes judgment, and judgment has no gate), but it means a skipped entry fails silently: the session looks identical either way, and the loss shows up only when someone goes looking for a rationale that was never written.
Treat the append as part of making the decision, not as bookkeeping after it.

Every decision taken under authority appends ONE entry, immediately, before acting on it:

```markdown
## Autonomous Decisions

### 2026-07-20T04:31Z - executor routing for the settings surface
**Chose:** `impeccable` for tasks touching `app/settings/**`, `do` elsewhere.
**Alternatives:** all-`do` (simpler, loses the a11y pass on a user-facing form).
**Why:** the plan's File Ownership Map lists three `.tsx` files; surface inference would route them anyway.
**Reversible:** yes - re-run the wave with `executor: do` if the polish pass is noise.
```

**Append to a DURABLE artifact: `{plan_path}.artifacts/COMPLETION.md` for a quick plan, or the plan folder's `COMPLETION.md` for a wave plan.**
Not `.fno/SUMMARY.md`: `.fno/` is gitignored and session-state files are explicitly transient and never archived, so a ledger written there dies with the worktree when it is pruned - taking the morning review with it.
The durable record is the plan artifact, the frontmatter stamp, `ledger.json`, and git history; the audit trail for decisions made on your behalf belongs with them.
**Before a plan exists** (a beastmode run is node-bound, but `/think` still decides in its early steps while the plan is written in its last), stage entries in the session scratchpad (`scratchpad_path`) as `autonomous-decisions.md`.
**Flush them to the plan's `COMPLETION.md` the moment `plan_path` is filled - not at ship.**
Deferring the flush is what makes the window dangerous: the scratchpad is worktree-local and gitignored, and the stop hook only archives it once a plan path exists, so entries left staged through a long do phase die with the worktree if the session never reaches its gate.
Flushing at plan-creation shrinks the exposure to the minutes between the first decision and the plan.
If the run ends with no plan at all, carry the ledger into the PR body under `## Autonomous Decisions`, and if there is no PR either, put it in your closing summary - a decision nobody can read afterward may as well not have been recorded.

Write each entry as its own append the moment the decision is made, never as an end-of-session batch: a morning review must read a complete list, and a session that dies mid-wave leaves whole entries behind rather than half of one.
Where an entry is genuinely uncertain, say so in **Why** - a recorded doubt is the thing the morning review looks for first.

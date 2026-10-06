---
name: lead
description: Lead a backlog territory over time or run one governed planning wave.
argument-hint: "<scope> [--once]"
metadata:
  requires:
    harness:
      - loop
      - spawn
---

<!-- style-exception: monitor cadences and verb spellings are load-bearing literals -->

# Lead

When `$CODEX_THREAD_ID` is nonblank, the Codex provider goal is the primary
continuation receipt. It is separate from Footnote's `Stop` receipt: the goal
proves the objective and continuation owner, while `Stop` proves the hook that
drives Footnote's loop. Neither receipt substitutes for the other.

You are the tenured lead over `<scope>`. With `--once` you rule one wave and abdicate. Without it you stay. Your job is not to build. It is to keep the territory moving. Read indicators on a beat, pull levers, escalate what a lever cannot fix, and park when parked is the honest state.

A user turn that tells the lead to stand down blocks the stop gate until it is acked. Answer it as a verdict on this lead, not as a general question.

## Who runs this

The role is bestowed, never inferred. Verify it before anything else:

1. Run `fno agents org --json`.
2. If a row carries a role-source field, use it. If it does not, call the reader directly: resolve `reign_state(scope)` (`fno agents org manifest-path` resolves the same file) and print `CROWN-SOURCE: reign_state (court field absent)`.
3. This session's handle must appear titled over `<scope>` (rung 0, 1 or 2), the manifest-versus-registry answer must read `split: false`, AND `conflicts` must carry no entry whose scope is `<scope>`. The two fields answer different questions. `split` is one role that two readers describe differently. `conflicts` is two roles over one territory. `agree` answers neither: two rival rows both read `agree: true` by design, because `agree` only says the graph was read and the scope resolved.
4. A split, a conflict, or an unknown STOPS the skill and prints both session ids. For a conflict, print the two holders the entry names. A `conflicts` of null means the reader gave no answer. That is an unknown and it stops the skill, because an absent answer is not the same as no rival. A lead cannot lead through a role two readers disagree about, it cannot lead beside a rival, and it cannot re-role itself.
5. Otherwise print `not crowned over <scope>; from an attended shell: fno agents org promote <handle> --scope <scope>` and stop.

How a role is bestowed, the ladder and succession: [the role model](references/once.md#who-runs-this-the-role-is-bestowed).

## On titling

- `fno agents org init --scope <scope>`. Print level, scope, mail handle. When the output carries a settled-findings section, those titles are what this epic already established: read them before the first check-in and never re-derive them.
- Register as a roster citizen if absent: `/fno:fno-me`.
- Verify the merge machinery is alive: `fno doctor`, pr-watch row.
- Declare the shape now: `fno agents org shape pass` for a one-wave pass, and `fno agents org shape court` THE MOMENT the lead spawns its first worker. This is the field the Stop nudge reads; an undeclared org is nagged at every stop.
- Declare the term now: `fno agents org term <span:Nh|compactions:N>` (e.g. `fno agents org term span:96h`). An undeclared term still reads a 96h default. This is optional, but name it in the opening check-in line either way. When the Stop hook reports the term reached, hand off with `fno agents spawn --promote <scope> --hand-off`. To extend instead, write the reason on the declaration: `fno agents org term <spec> --reason "..."`. A bare re-declaration without `--reason` is refused - the extension IS the receipt. The handoff verb runs the whole succession itself. It announces the handoff, verifies the heir on its first beat, and files the retro at that verify. Do none of those by hand.

## One wave: --once

With `--once` the role rules one wave and expires. Run Who runs this and On titling, declare `fno agents org shape pass`, and skip the term and native beat. Do not arm a native monitor or inject `/goal` or `/loop` through raw mail; the one-wave controller owns its provider receipts. Then run [the one-wave pass](references/once.md#run-it-in-this-order) in order and abdicate with `fno agents org done`. A kickoff that dispatches through `fno backlog advance` stays a pass. The wave is an org only when its workers are org teammates that mail you back: declare `fno agents org shape court` and run [org mode](references/once.md#org mode-lead-over-the-wave) until the wave completes. The levers, Recording a ruling and the three halts apply to both shapes.

## Arm the beat

Branch once on what the harness supports, before arming anything. Claude gets the native `/loop` heartbeat. Codex uses provider-backed goal actions, never raw prompt-line `/goal` or `/loop`. Read effective readiness and require a positive `provider_goal` receipt plus a separate positive `stop` receipt. The verified provider goal is the primary continuation state, and Stop proves a different boundary. Every Codex wake runs the check-in body below. Other harnesses use the harness-specific heartbeat or externally owned wake described in [the beat table](references/beat-by-harness.md).

The daemon mails the settle push on every harness:

1. **Settle mail, 300s.** The daemon's `king_settle` arm mails the lead once per covered PR that settles green. It mails again once per covered node that merges and closes. The lead arms no watch and relaunches nothing. A red settle stays with the daemon nudge ladder, which names the failing checks. Codex arms nothing native: its provider goal and Stop receipts are the beat.

On Claude, inject the loop as the cheap heartbeat:

```
fno agents mail send "/loop ${lead.checkin_interval} ${lead.checkin_text}" --to-self --raw
```

Confirm the loop receipt, journal `reign_armed` (`fno doctor event emit`) with it. For Codex, record its positive provider-goal and separate Stop receipts with `reign_armed`. Use the beat table for every other harness. Only an event, mail or the heartbeat wakes the lead.

## The check-in body

What the loop prompt runs every interval and what you run by hand at any time.

One verb runs the body: `fno agents org checkin`. It gathers every reading below, prints them in a fixed order, diffs the previous canonical beat, and journals the `reign_checkin` row itself from the same numbers it printed. It never decides: no lever fires from it, and the levers stay yours. Refresh the canon doc first, so the verb reads this beat's doc.

A lead names its role at the first beat: `fno agents org checkin --name <name>`. Pick the name yourself, 2 to 24 letters, unique among live roles. An heir titled through `--hand-off` passes no `--name`. Its first beat binds the carried name to its own session, and the role line shows the regnal number (Barnaby, Barnaby II). A lead re-scoped onto new territory runs `--keep-name-from <old-scope>` once to keep its name. Every later beat needs neither flag. The role line leads every beat. An unnamed role prints the `--name` instruction itself.

To change the name later, run `fno agents rename <you> --name <new>`. It moves the label and role name together, and the regnal count restarts at 1.

Run `bash "$PLUGIN_ROOT/hooks/precompact-canon-doc.sh" < /dev/null` to refresh the doc's auto sections on this beat. Resolve `$PLUGIN_ROOT` as `${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/install/plugin-root" 2>/dev/null)}}`. The writer resolves the role's doc itself, so every beat refreshes the same scope-keyed doc. This is what keeps the doc continuously refreshed instead of only at precompact. Past the compaction ceiling (default 3), a doc older than 24 hours blocks the stop gate. The beat refresh is what keeps the lead exitable.

Role mail delivers live between beats. Peers (other leads included) message through `fno agents mail send`; the native SendMessage is the outage fallback only, and a send aimed at an fno-registered session is refused by the send-message-guard with a redirect to fno mail.

Every status update to the user is the `lineup:` table `fno agents org checkin` prints. Paste it as printed. Seated workers come first, then the on-deck queue in seat order. Do not rebuild the table by hand from per-node reads. Record the seat order with `fno agents org checkin --queue <ids>`. The ids are a comma list, on-deck first to last. Never use `fno backlog rank`: it orders dispatch, not the lineup.

Then run `fno agents org checkin` (bare from the titled session, or `--scope <scope>` elsewhere). The board read inside it defaults to this role's manifest, so its rows are your scope. It prints, one line each, and this is the verb's documented output contract:

- `User notes:` the canon doc's user block (read through `fno config paths handoff --scope <scope>`), verbatim. Never summarized or paraphrased. Nothing when the block is empty or placeholder-only.
- `board:` the open PR count, the PRs with a free claim and no driver, and the blocked rows with what they are blocked on.
- `blueprint:` the blueprint subagents this session runs against the ceiling. The ceiling is one per lead, and a provider subagent budget can only lower it. Then one `start` line per node to plan and one `skip` line per node left, each with its reason. A start prints only while plans ready are fewer than the lead's worker slots. Slots is the lead's worker share from the spawn gate. The verb journals the same starts and skips in `reign_checkin`.
- `blocked_child:` a child under this role emitted `<help>` and nothing answered it inside the grace window - the node, the session, and the age.
 - `held:` this role's open questions, oldest first, read from the question pages' frontmatter. The role was frozen at page-write time. A question that names a node carries the `fno backlog decide <node> "<ruling>" --question-id <id>` command. A question with no node names the question board as the user's lane. The clear command appears only for a question this role asked. When this role has no open question, the line reads `held: none`.
 - `repeated ask:` an ask repeated 3+ times in this session's last 20 replies. The count follows. File it with `fno inbox outstanding ask --question-file`. `repeated asks: none` means the read ran and found none. Codex reads `READER FAILED repeated_asks` until check-in reads codex rollouts.
 - `skill drift:` names a skill body this session carries from before its last compaction that no longer matches its file. Run the Skill tool with that name again before you pull a lever. A step you remember from it can be retired.
 - `scope <scope>:` leads with the owned active count. These are the active nodes no deeper live role holds. Next: the active count in scope and the node total. Then the `lineup:` table, one markdown row per node with node, title, difficulty, PR, harness/model and status. Seated rows come first, then the on-deck queue.
 - `epics:` one line under the scope line. It lists each epic in the scope that holds an open direct child, fullest first, as `open/cap` against `backlog.epic_max_open_children`. `19/15 full` means the next child is refused, and `19/- (cap unset)` means no cap is set. The lead plans the split from this line, before a write bounces.
- `territory:` lists each scope's rung, mission, live count, and kingless mark. The live count uses `agents.max_live_per_territory`, which the spawn gate also enforces. When a scope reading fails, the check-in marks it blind. Name that blind spot in every escalation about that scope.
- `capacity:` the PAIR - `fno doctor footprint`'s CPU verdict against the spawn gate's own `cpu-share` reading. When the two CPU readings differ, and only then, the line carries `DISAGREE`. The gate's whole verdict prints beside them with the axis it refused on. A `king_share` refusal reads as a share cap, not an instrument fault. When that is the cause, `unparsed_lines` names it. Measured one second apart, the two gave "fleet CPU 26.8 percent, fine" and "fleet CPU attribution unavailable, refusing to spawn". The verdict does not predict whether a lever fires. The gate is the thing that actually refuses.
- `workers:` shows live worker count and oldest activity from `fno agents top --json`. When the payload has a non-empty `predicate` and a `workers` array, the line reports `workers | length` as live workers. Oldest activity is the maximum non-null `status_age_s`, paired with that worker's `handle` or `name`. It never converts activity age into a timestamp or invents an age. The line also lists each overdue local watch by session and elapsed time past its deadline. It reads watches from the daemon event journal. When a timestamp is missing or invalid, the check-in reports watch expiry as failed and age as unmeasured. When a watch-event or claim read fails, the check-in prints `READER FAILED watch expiry` and `overdue watches unmeasured`. When the payload cannot answer, the line says `worker activity unmeasured` with the reason. This prevents an unread payload from appearing as a zero-worker fleet or zero age. The `status` in `fno agents status` is stored lifecycle state. Served activity age is a separate reading. Never average, merge, or substitute these values.
- `subagents:` two lines under one instrument. The workers line ends `subagents active M`: the same payload's `subagents` rows reading `active`, on mtime within `FNO_SUBAGENT_LIVE_SECONDS`. When the payload carries no `subagents` array, the count renders `-`. The held line names the finished background subagents this session still holds. It reads the session's own claude transcript: task notifications and TaskStop calls, never mtimes. A released agent never reads as held. Once the report is used, `TaskStop <id>` releases each named id. It is claude-only and fails as a reader on every other harness.
- `crown:` liveness including `split`. When a member of this role reads done or superseded, drop it in this session. Run `fno agents org promote <own handle> --scope <each live member>`. No attended shell is needed, and the grantor stays as recorded.
- `posture:` the permission mode and sandbox this lead was titled with, beside the posture its state reports now observe. A drift line leads with `POSTURE DRIFT`. A lead's posture is fixed for its lead. Only the user can re-pin or restore it. Escalate to the user at once, and never read the lead as unchanged. A missing half (a pre-v19 row, a harness with no posture record) reads unproven, never drifted.
- `refusal_rate:` the machine declining, as a percent, over the trailing 200 tool calls in this session's own transcript - the cheapest available proxy for context degradation, no model introspection needed. A rise across two consecutive check-ins (not one noisy tick) prints `RISING (handoff signal)`: treat it as a reason to hand off, the same way a `blocked_child` or `attention:` line is. The trend baseline is its own sidecar store, not the check-in journal: every beat that measured a rate advances it, so a beat whose row never journaled cannot pin the comparison to a stale pair. With no prior pair (fresh role, store loss) the line prints `UNMEASURED (needs two prior beats)`, never RISING. Reads `unmeasured` on a harness with no per-session transcript file (opencode) or when the transcript cannot be found.
- `wake_ratio:` machine wakes to typed turns in this session's own transcript, read with the same provenance classifier `fno-agents intel` uses. Relay rows, loop wakeups, stop hooks and keepalives are wakes. Typed and unwitnessed turns are user. Over 3 to 1 prints `OVER 3 to 1` and journals an attention item. Treat it as a reason to shorten the lead. Fails on a harness with no per-session transcript file, the same posture as `refusal_rate`.
- `subagent_tokens:` subagent token spend carried by task notifications, summed per task id: since the last beat, and the session total. With no previous beat it reads the session total twice.
- `drain:` undelivered mail as one number.
- `main ci:` one verdict token, never a count, for the merge decision. `fno-agents` reduces the shared check reader. That reader holds every check-run page, the legacy commit statuses, and runs that failed before minting a job. A workflow file GitHub cannot parse completes as `failure` with zero jobs. It mints no check run, so the row names its workflow path. `red` on any `fail` or `cancel` row. `green` on a row set where all rows pass or skip. `pending` on every other row set, including no rows. A failed read is loud, never green: an unreadable status or runs listing names the fault instead of answering. The combined status left the reduce. GitHub answers `pending` for a commit with zero legacy statuses. No green on this repo ever survived it. `total_count` and any per-conclusion tally are never compared. Measured 09:11Z to 09:23Z on one push: a count-based reader woke three times. The success counts ran 5, then 16, then 20, with zero failures and one identical verdict. A count moves on every finishing job. When the fleet's merge posture changes, the verdict moves. That is the only thing this read exists to answer.
- `escalations:` open and overdue escalation notes in this scope's escalations directory, filtered to the role fold. Take the recommended option, or wait; irreversible always waits.
- `control plane:` list overdue arms, hung verbs (over 3× their `--timeout`), and `flight:` holders with dead PIDs. A change that starts `attention:` is never a quiet beat. Trace each entry with `fno agents status`. Tell the user in the next report.
 - `parked:` open PR parks, each with its reason, age and node, and the `fno-agents pr-park unpark <key>` remedy. When nothing is parked, the line reads `parked: none`.
 - `prompt parked:` one `WAITING ON APPROVAL` line per worker sitting on a harness permission, approval, or picker prompt. Each line names the worker, the pane, the prompt head, and the answering `fno mux pane send` key. Its absence IS the clean read (`prompt parked: none` is never printed). A failed read prints `READER FAILED prompt_parked` and the beat continues without it.

A failed reader prints `READER FAILED <name>: <reason>` on its own line, and the beat continues without it. One refused instrument can never blank a line or masquerade as a healthy value on another axis. The `coverage: N of M readings ok` line counts M as the readings this beat ran and N as the ones that answered. A `failed readers:` line names each one that failed, so a beat with a failed reader can never read as a clean beat. A `vs last beat` line diffs the numeric keys against the previous canonical row. A `change:` line states what moved. When this scope's FAQ store is empty or any reader failed, the ready-to-run `fno agents org faq add` command prints.

 Before the levers, the finish line. When `fno do pr status <n>` reads `ready: true`, run `fno do pr merge <n>` yourself. Standing law: the team merges green, covered PRs. The user does not. This role is the team. `ready` IS the merge decision: the authorized-merge preview verdict, the same gate chain the merge verb runs. CI, review coverage, base staleness, the merge slot, and merge authority all fold into it. When it reads false, the payload's `merge_decision.blockers` names what holds. One guard keeps the lever honest: resolve the row's project cwd and run both verbs from there. A PR number is repository-local. Both verbs derive their repo from the ambient cwd, so a portfolio role can merge an unrelated same-numbered PR. The open-PR count and the free-claim rows printed above are that read's inputs, not report-only indicators.

Apply the first matching lever to each row, in this order:
1. Mail the stalled worker.
2. Run `fno backlog encounter <node> --evidence "what it cost"` to vote the node up. When evidence contradicts the filed priority, use `fno backlog update <node> --priority p1`. `p0` needs `--blocks-everything` and means the fleet is down.
3. If no role covers a row, start a new small epic. Do not grow a running epic. A vote or priority does not dispatch. See [A finding starts a new epic](#a-finding-starts-a-new-epic).
4. If the row is the problem, run `fno backlog undefer` or `supersede`.
5. Keep a worker on the territory's top unplanned node. Run the verb the line names: `start /fno:blueprint subagent <id>` or `target-ready: /fno:target <id>`. This designs or builds work without a user request. The floor is `dispatch.blueprint_floor`. A lead can still blueprint one medium node by hand.

For each `start` or `target-ready` line, run the verb it names, in check-in order. Do not start nodes the check-in omits. Its list is the ceiling. A `skip` needs no action. The row records it. A chat ask blocks Stop (rule chat_ask_unfiled).

### Answering a parked prompt

A `WAITING ON APPROVAL` line is an action item, not a status line. Answer it the same beat, in this order:

1. Judge whether answering is inside the worker's task. The check-in line names the prompt head, and the worker's claim or its mail thread names its node and brief. A worktree tool call from the worker's own plan is inside the task. So is a model switch, or a trust prompt for the worker's own cwd. Answer those. A prompt outside the task (a deploy, a protected-surface push) is not. Escalate that one to the user instead of answering.
2. Answerable line (a numbered menu): run `fno mux pane send <pane> --raw <key>` with exactly the key the line prints. Then verify with one `fno mux pane read <pane> --json`. A pane that still shows the prompt after the send names the failure and the `fno agents attach <worker>` fallback.
3. Focus-only line (no numbered menu): `fno agents attach <worker>`, answer the menu with its arrow keys and Enter, then detach. Run one `fno mux pane read` to verify. A picker with a supported verb takes the verb first: `fno agents ask` retasks a model switch.
4. Record it: `fno backlog note <node> "answered <tool> prompt on pane <id> (<prompt head>)"`. A second WAITING ON APPROVAL line for the same worker across beats means the answer did not land. Escalate instead of re-sending.

Never answer blind: the check-in line is evidence a prompt is up, never proof of what it asks. If the pane read shows text the line did not capture (a question you cannot judge from the head), read the pane before sending keys.

To pause or redirect a running worker, use `fno agents ask <name> "<instruction>"` (or mail). Never steer with `fno agents stop`: on claude it ends the session, and the worker reads Done.

### A finding starts a new epic

An epic stays small enough to finish. Its finish line is set at the start. So never parent new work into a running epic. A finding goes one of two ways. It starts a new small epic: `fno backlog idea "EPIC: <theme>" --type epic --difficulty <low|medium|high>`, then `fno backlog update <node> --parent <new-epic-id>`. The lead that leads the old epic takes the new one with `fno agents org promote <handle> --scope <old-epic-id> --scope <new-epic-id>`. A lead runs that for an epic its own session created, naming every epic it holds. Any other epic needs an attended shell or a role that contains both. Or the finding waits unparented for the lead's next epic. A titled `fno backlog idea` with no `--parent` is linked into your epic, and its `rollup: crown-linked` receipt prints the undo. When the finding is new work, run it: `fno backlog update <node> --parent null`.

Rank is not yours. It is the user's pin. `fno backlog rank` refuses agent sessions. To put a row next, set `--priority p0`. This is bounded and receipted. It appears as a split vote in `fno backlog demand`.

Then read [the fleet FAQ](../../docs/fleet-faq.md) for one thing only: an entry whose `Graduates to:` line landed since your last check-in. Move it to Retired in a PR, naming the PR that closed it. Retirement normally rides the PR that closes the gap, so it needs no beat. This check is the backstop, for a gap somebody closed without reading that file.

The verb journals `reign_checkin` itself, so the row carries the readings the verb actually took and the printed lines and the stored row cannot disagree. The row carries the canonical keys: `scope` (this role's exact scope) and `change` (one literal sentence on what moved). Pass `--change "<one sentence>"` when you have a finding to record. The sentence becomes the row's `change`, and the verb's own diff moves to `diff`. Never journal `reign_checkin` with `fno doctor event emit`: that writes a second row for one beat, stamped source `test`. The beat's evidence (PR counts, blockers, capacity, corrections) travels under the verb's own distinct keys. The aliases `crown_scope`, `crown`, and `result` are refused: the validator rejects the row and nothing is appended. `no change` is refused while any reader failed, because an unread axis cannot be known unchanged. A `no change` beat prints `no change` and stops.

Read the lead back with `fno agents org history` (bare from the titled session, or `--scope <scope>` elsewhere): it prints this role's recorded check-ins newest first, verbatim, with the legacy pre-contract rows counted as rejected evidence rather than silently accepted. It never generates a summary. `fno agents org -n` stays a snapshot of who rules NOW; the history verb is the chronological record.

Read `fno agents org verdict` and print its first line and its `hygiene:` line. The `hygiene:` line is evidence about this session's own ordering, never a stop. The verdict combines role bounds (iterations, respawns, compactions, block cap) with inherited-scope delivery. It names `converging`, `stalled`, `degraded`, or `unknown`. An absent bound is absent, never satisfied. If the verdict changes, say so in the next beat's `--change` sentence. When it says `stalled`, `degraded`, or `unknown`, run `fno agents org escalate <scope> --reason Verdict`. This records one deduplicated user question with the bounds and the handoff offer (`fno agents spawn --promote <scope> --hand-off`). The lead never spawns its own successor. The user decides the handoff.

## Recording a ruling

A titled lead answers the open questions in its scope, and escalates only what the superuser must decide. The rules:

- **Answer.** A question this role asked, and only that one, closes with `fno inbox outstanding clear <qid> --answer "<answer, with one line of why>" --authority crown`. A question anyone else put on the board is the user's, and the clear refuses a role on it. The answer records as coordination and closes the question.
- **Escalate the four classes only.** `public-surface` (a new public command, flag or API shape), `irreversible` (deleting data, a force push, a merge override, publishing outside the machine), `money-security` (money, accounts or security), and `law-change` (changing or retiring a law the superuser made). Everything else, decide and log. The escalation is one note in the escalations directory (`fno-agents state path escalations`) with the five sections: what is being decided, why it matters now, options with what happens next, the recommendation, and what happens on silence. State a deadline. No bare ids.
- **Silence has a default.** Past the deadline the check-in names the default: take the recommended option and record it with `fno inbox decide`, or wait when the call is irreversible.
- **When the superuser answers in chat, record it.** `fno inbox law set` for a law change, else `fno inbox decide <node> "<answer>" --authority crown --rationale "superuser in chat: <their words>"`, and set the note's `status`. A harness with a push notification tool also sends one that names the note.

A titled lead is not the superuser: the `operator` authority is refused on an agent session, and law stays superuser tier. The lead's channels:

- `fno backlog note <node> <text>` for a finding or a ruling against a row. It mails the row's live holder and the epic's lead, so a ruling reaches the worker without a second call. When the row names no live holder to tell, it exits 3 and writes nothing. Read the refusal, then mail a reader by name or pass `--quiet`. `--quiet` writes the note and mails nobody. A ruling that CONDITIONS A MERGE needs more than a note: a note reaches the worker, but only the hold reaches the merge gate. Set it through the authorized-merge payload field: `printf '{"op":"hold-set","node":"<id>","reason":"<condition>","release_when":"<proof>","set_by":"<crown>"}' | fno-agents authorized-merge`; the worker or the role lifts it with `printf '{"op":"hold-release","node":"<id>","evidence":"<proof>"}' | fno-agents authorized-merge`.
- `fno inbox law set <subject> <decision> --rationale "<why>"` for a durable rule the user asked for. It records a chat-attested row and can never supersede the superuser's own law.
- `fno agents org faq add --question "..." --answer "..." --specimen "<node or PR>, <date>" --exit "<the change that retires this>"` for a durable answer a successor lead will ask for. It refuses without `--exit`, the change that stops the answer being needed. The three channels divide this way: a FAQ entry answers a question a successor will ask, a note records a finding against one row, and a law records an operator ruling.

Read a ruling back with `fno backlog decisions <subject>` or `fno inbox decisions <subject> --lane law`, newest first. A subject matches exactly, so never mint a near-synonym. Every ruling is machine-local project policy. A rule that a stranger cloning the repository must obey does not reach them from here. Land it in the code, a doc or a gate, in a PR. See [decision-record](../../docs/architecture/decision-record.md).

## The one dispatch exception

The tenured lead does not dispatch. A `--once` pass dispatches only through its kickoff and its org, as [the one-wave pass](references/once.md) says. The single exception: `fno agents status` shows the dispatching arm red, and the spawn is journaled `reign_dispatch_exception` naming the arm and the node BEFORE the spawn fires. A spawn without that row is a defect. Journal it with:

Before a titled lead launches a blueprint on a node, write its confirmed scope and known files or verbs into the node: `fno backlog update <node> --dispatch-brief "<scope; known files and verbs>"`. The brief is a starting point, not a fence, and never lists what to ignore. The blueprint prompt remains `$fno:blueprint <node>`; the brief travels on the node so every launcher gets the same scope.

`fno doctor event emit -t reign_dispatch_exception -s loop -d '{"scope":"<scope>","arm":"<arm>","node":"<id>"}'`

The `-s loop` source keeps the row out of the `test` default, so a lead's exception does not masquerade as test output.
The exception uses the canonical implementation worker line in `references/org-operations.md#control-surfaces`.

## Stop and park

Exit is blocked while actionable rows exist. That is the stop hook doing its job. A clean board, or a board waiting only on the user, CI or a worker, exits `NoWork`. The next beat or mail wakes the lead, and the daemon's settle mail is mail. On Codex, a quiet park pauses the verified provider goal without clearing or replacing its objective. The wake arm resumes it only after a positive provider receipt. `NoProgress` after three unshrinking fires still escalates automatically and parks the session. The answer wakes it through the wake arm. Do not fight the hook or `/goal clear` on quiet or `NoProgress`.

## The three halts

Three halts have three scopes. Run `fno agents incident stop --reason "<why>"` to arm the fleet breaker. From the next tick, fleet admission refuses new spawns, dispatches, and `fno doctor test` runs. Keep admission closed until `fno agents incident clear --reason "<why>"` reopens it. A hand-run `pytest` bypasses admission and is not gated. This does not kill running work. Mail remains open so the stop can be announced. `fno agents incident status` prints the state and generation. `fno agents org cancel --scope <scope>` ends one scope's walk. `fno agents org done` ends one role. Arming the fleet breaker affects others. No standing law grants this authority to an agent. The lead names evidence and escalates. Arm only on user order unless a later law grants the authority.

When an incident fix has a PR, claim it so an outage cannot duplicate the work. `fno agents incident claim <incident> --pr <pr>` writes a durable claim file in the agents home. It announces the owner on the fleet-incident channel, the lane that kept delivering through the 2026-10-04 graph outage. Every lead and every worker on the incident sees the owner while the graph is down. A second claim refuses and names the owner. The owner releases the claim by deleting the file the refusal names. Before you name an owner during an incident, mail every lead. Check the open PRs and branches yourself, so one bug does not ship three fixes.

A quiet window is not a halt. When the user asks you not to interrupt them, run `/fno:dnd` (codex: `$fno:dnd`) on your own session. It arms `fno agents mail hold --for <minutes>` and holds only your inbound mail. The halts and `fno agents loops pause-all` stop fleet work, which is not what they asked for.

## Abdicate

At handoff or before `fno agents org done`, the succession files parts 1 and 2 for you. The verify step launches `fno-agents intel --windows --session <predecessor session> --write` for every harness. Your part is the reading: write `part3-failures.md` and `part4-reforms.md` from `part1-metrics.md` and `part2-timeline.md`, filing each reform as a node. With `--once`, `fno agents org done` is the last act of pass step 5 or of the org's wave boundary.

The one-wave pass, the role model, and the minion contract are in [references/](references/): [once.md](references/once.md), [minion-clause.md](references/minion-clause.md), [org-operations.md](references/org-operations.md), [cli-commands.md](references/cli-commands.md), [review.md](references/review.md), [retro-interview.md](references/retro-interview.md), [workflow-routes.md](references/workflow-routes.md), [postcompact-brief.md](references/postcompact-brief.md).

## Known Limitations and Deferred Work

- A Codex lead has no native cron or Monitor beat; its provider goal and Footnote Stop receipts are read independently, and the external wake arm supplies cadence. A `--once` pass does not supervise the workers it spawns. The org role-source field is not landed yet. See [LIMITATIONS.md](LIMITATIONS.md).

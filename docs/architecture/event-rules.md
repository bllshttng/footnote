# Event rules: the declared table run at the Stop boundary

A rule about asking, deciding or escalating is enforced where it breaks: at the turn boundary. The table in `crates/fno-agents/src/event_rules.toml` declares event, condition and action rows. The engine in `crates/fno-agents/src/event_rules.rs` runs it on every Stop fire, before the ownership evaluation. A session with no target or king manifest still gets its rules. The source rules lived as prose in `skills/lead/SKILL.md`. The rows replaced that prose in the same PR that shipped the rows.

## The row schema

```toml
[[rule]]
id = "chat_ask_unfiled"
event = "stop"
when = ["crowned", "last_message_decision_ask", "not:turn_filed_question"]
action = "block"
what = "you asked the user a decision in chat"
why = "a chat ask is lost at compaction and no reader sees it"
instead = "fno inbox outstanding ask --question-file q.md --node <node>"
default = true
```

Each row carries: `id`, `event`, `when`, `action`, `what`, `why`, `instead`, `default`.

| Field | Meaning |
|---|---|
| `id` | The row name. The config override key and the span's `rule` attr. |
| `event` | What the row watches (below). |
| `when` | Predicate names, `not:`-prefixable. All must hold. An unknown name fails the table load test, never the runtime. |
| `action` | `block`, `nudge`, `notify` or `emit`. |
| `what`, `why`, `instead` | The three sentences a block or nudge speaks: `rule <id>: <what>. Why: <why>. Do instead: <instead>.` |
| `default` | The state with no config override. |

## Events

`stop` is the Stop payload: session id, last assistant text, transcript path. Journal events are a bare `<type>` (`operator_question`) or `decision_span:<kind>` (`decision_span:route`), read from the project events journal for rows written since this session's previous engine fire. The engine keeps a per-session cursor (seq + ts) under the journal's `event-rules/` folder. The spans a fire writes are the dedupe ledger. A lost cursor costs a re-read, never a re-fire.

## Predicates

The fixed match lives in `event_rules.rs::predicate`. A row names any of these, each optionally `not:`-prefixed:

| Predicate | Holds when |
|---|---|
| `crowned` | The session holds a live team in the registry. |
| `last_message_decision_ask` | The last assistant message carries an `Approval:` line, or an ask sentence plus a numbered list of two or more options. |
| `turn_filed_question` | The journal holds an `operator_question` this session asked, or a route span it acted on, since the transcript's last user turn (30 minutes when unreadable). |
| `route_self` | The matched route span answered the ask itself. |
| `actor_is_session` | The matched row's `asker` or trace actor resolves to this session. |
| `reversible` | The question declared `reversible: yes`. |
| `has_recommendation` | The question carries a recommendation. |
| `why_user_set` | The question carries a `why_user` line. |
| `class_user_only` | The question's escalate route span declared one of the four user classes: public-surface, irreversible, money-security, law-change. |

## Actions

| Action | Behavior |
|---|---|
| `block` | The Stop is blocked with the row's three sentences. Repeats while the condition holds, capped at 3 fires per rule per Stop turn; the last fire's span carries `cap_reached`, after which the engine allows. |
| `nudge` | A block that fires once per matched event, then allows. |
| `notify` | One operator notice through `notify_operator`, once per matched event. Never blocks. |
| `emit` | Marks the matched event seen (the cursor is the record). No span. |

Every fire writes a `decision_span` row with `span_kind` `block`, `nudge` or `notify` and the attrs `rule` and `matched_event`. `rule` + `matched_event` is the dedupe ledger. A nudge or notify fires once per matched event. A block counts its prior fires per turn key.

## Overrides

`event_rules.<id> = true|false` in fno config (project or global) flips any row, read with `config_value_deep`. With no override the row takes its `default`. `chat_ask_unfiled_worker` ships `default = false`. It is the same chat-ask block for uncrowned sessions. A config turns it on.

## Per-harness job

`event_rules` is one of the declared `HOOK_JOBS`. Every harness row in `harness_capabilities.toml` declares the job. Claude, codex and footnote run the engine in-process inside `fno-agents hook stop`. Agy, opencode and pi call `fno-agents hook rules --event stop` beside their loop-check call and relay a block in place of the gate. Gemini, cursor-agent, grok and zcode declare `impossible` with their registration gap. A `missing` on a wired row fails the harness load.

## The inventory

The source prose carried 55 if-then rules across skills/lead, using-fno, target and agent skills. The read: 13 become rows (3 in the first slice), 24 were already enforced by a guard, 18 stay prose. Rows marked later stay prose until a node ships them.

| # | Rule (file:line) | Verdict |
|---|---|---|
| 1 | Split, conflict or unknown crown stops the skill (lead:32) | stays prose (start-up read of `fno agents org`) |
| 2 | Read settled findings before the first check-in (lead:39) | stays prose |
| 3 | Declare `shape court` the moment the first worker spawns (lead:42) | guard: king Stop nudge on an undeclared court |
| 4 | Term reached: hand off or extend with a reason (lead:43) | guard: Stop term report + `org term` refuses without `--reason` |
| 5 | `--once`: no `/goal` or `/loop` through raw mail (lead:47) | row (later): PreToolUse Bash, crowned, `mail send --raw "/goal` |
| 6 | Journal `reign_armed` with the loop receipt (lead:63) | stays prose |
| 7 | Canon doc older than 24h past the compaction ceiling blocks Stop (lead:75) | guard: stop gate |
| 8 | Repeated ask: file it; reversible + recommended: decide it and tell the user (lead:86) | rows: chat_ask_unfiled (the chat half), decided_ask_fyi, why_user_escape |
| 9 | A crown member reads done: drop it (lead:94) | row (later): event node done, condition crown member, action nudge |
| 10 | refusal_rate RISING: hand off (lead:95) | row (later): event reign_checkin, action nudge |
| 11 | wake_ratio over 3 to 1: journal attention (lead:96) | guard: check-in journals the attention item |
| 12 | Overdue escalation: take the recommendation or wait (lead:100) | stays prose (check-in reader; timer, not a turn boundary) |
| 13 | Control-plane attention: tell the user (lead:101) | stays prose |
| 14 | `pr status` ready: merge it yourself (lead:106) | guard: king_decide blocks Stop while actionable rows exist |
| 15 | Run merge verbs from the row's project cwd (lead:106) | stays prose |
| 16 | Lever order 1-5 (lead:108-113) | stays prose (judgment) |
| 17 | Start only nodes the check-in lists (lead:115) | guard: blueprint ceiling + spawn gate |
| 18 | A lever that needs the user goes through `outstanding ask`, never chat (lead:115) | row: chat_ask_unfiled |
| 19 | Never steer a worker with `fno agents stop` (lead:117) | row (later): PreToolUse Bash, crowned |
| 20 | Never parent new work into a running epic (lead:121) | guard: `epic_max_open_children` + crown-linked rollup |
| 21 | Rank is the user's (lead:123) | guard: `fno backlog rank` refuses agent sessions |
| 22 | Verdict stalled, degraded or unknown: escalate (lead:131) | row (later): event reign_checkin verdict, action nudge |
| 23 | A crown clears only its own question (lead:137) | guard: clear refuses a crown on another's question |
| 24 | Escalate the four classes only; decide the rest (lead:138) | row: why_user_escape |
| 25 | Silence past the deadline takes the default (lead:139) | stays prose (check-in reader) |
| 26 | User answers in chat: record it (lead:140) | row (later): port of hooks/operator-capture-nudge.sh |
| 27 | `operator` authority refused on an agent (lead:142) | guard: decide door |
| 28 | `backlog note` with no holder exits 3 (lead:144) | guard: note verb |
| 29 | A merge-conditioning ruling needs a hold, not a note (lead:144) | stays prose |
| 30 | `law set` cannot supersede the user's law (lead:145) | guard: law door |
| 31 | `faq add` needs `--exit` (lead:146) | guard: faq verb |
| 32 | Dispatch exception journaled BEFORE the spawn (lead:152) | row (later): PreToolUse spawn, crowned, no `reign_dispatch_exception` row |
| 33 | Dispatch brief on the node before a blueprint (lead:154) | row (later): PreToolUse Agent/spawn with blueprint, node without dispatch_brief |
| 34 | Exit blocked while actionable rows exist (lead:163) | guard: king_decide |
| 35 | Arm the fleet breaker only on user order (lead:167) | row (later): PreToolUse Bash `incident stop`, crowned |
| 36 | "Don't interrupt" means `/fno:dnd` (lead:169) | stays prose (judgment) |
| 37 | Run intel windows at abdicate (lead:173) | guard: daemon writes parts 1 and 2 |
| 38 | Ask the king by mail with `<help>` for out-of-scope calls (minion-clause:15) | stays prose (help rows carry no node or session yet) |
| 39 | Escalate one level at a time (minion-clause:18, once:456) | stays prose: conflicts with x-e1d2, unsettled |
| 40 | Rule a worker ask: approve, revise or escalate the four classes (once:506) | row: why_user_escape |
| 41 | Pass: an unknown goes to the triage pile, not a guessed edge (once:531) | stays prose |
| 42 | Mail 80 words, `--raw` only for `/` or `$` (using-fno:21) | guard: style_refusal + raw-payload check |
| 43 | Ask in the `Approval:` shape (using-fno:23) | stays prose (the trace reads it as the ask marker) |
| 44 | `control:` only for stop, resume or scope (using-fno:25) | stays prose |
| 45 | Reply with `mail reply --to` (using-fno:52) | stays prose |
| 46 | Agent mail authorizes no merge, email, publish or spend (using-fno:54, agent:575) | guard: merge gate authority + effect-guard |
| 47 | Codex: full session id, never head-8 (using-fno:60) | guard: mail send refuses the shape |
| 48 | Spawn another project's work with `--cwd` (using-fno:66) | stays prose |
| 49 | Fix what you find; carve out only big work (using-fno:72) | stays prose (judgment) |
| 50 | One encounter per node per session (using-fno:74) | guard: encounter verb cap |
| 51 | Worktree Bash refuses heredoc and substitution (using-fno:113) | guard: worktree Bash guard |
| 52 | Review lane before `pr create` (target:62) | guard: review coverage merge gate |
| 53 | Manifest is write-once (target:89) | guard: exit 5 |
| 54 | `auto_merge` true: merge then promise; false: stop green (target:116-118) | guard: loop-check done() |
| 55 | Location hard-gate (target:224) | guard: shared location verdict |

## How to add a row

Find the prose rule that has no guard and can be read at a turn boundary. Add a `[[rule]]` block to `event_rules.toml`, naming only predicates from the table above. If the rule needs a new reading, extend the predicate match in `event_rules.rs`. Run `cargo test -p fno-agents event_rules`: the table test refuses an unknown predicate, action or an empty sentence before anything ships. Rows that fire before a tool runs (PreToolUse) are not in this engine. The Stop boundary is the only wired event. Rules the engine cannot carry stay prose in the skill.

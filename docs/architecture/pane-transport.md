# Pane transport: what a typed message carries, and what its receipt claims

`fno mux pane send` types keystrokes at a worker's prompt. It is the most reliable transport footnote has. It used to be the only one carrying no attribution. This doc states what it carries now, what each receipt word means, and why `fno mail send --force` is opt-in.

## The defect, measured

On 2026-08-21 an operator read a king's instruction in pane 45 and asked why it had been sent `--raw`. It had not. `fno mail send` returned `queued (durable) [live-miss]`. The king fell back to `fno mux pane send --text ... --submit`. The message arrived with no sender, no message id, no reply handle, and no authority footer. In the pane it is indistinguishable from the operator typing. A worker cannot tell a peer's dispatch from an operator order. The two carry different authority. A peer cannot authorize an outward action the operator did not. The footer is the only thing that says so.

The same night, mail live-missed repeatedly against a codex worker under both its registry name and its full session id. The pane path landed 4 of 4. So the transport that worked was the one carrying nothing. The design call is to wrap the durable path, not to route around it.

## The envelope is the default

`fno mux pane send <pane> --text "..."` wraps the text in the same `<fno_mail>` envelope the mail lane produces. `--raw` opts out.

It is not `--envelope` to opt in. An opt-in flag leaves every existing caller unattributed and fixes nothing. Defaulting fixes every caller at once and forces the small set of genuine keystroke callers to say so. Those callers are real, not a courtesy. Answer an option prompt with a digit. Send a bare control key. Type a shell command. Clear a modal. An envelope around the character `1` is nonsense.

An earlier ruling said "an envelope typed as keystrokes is still keystrokes" and dismissed this. That ruling is withdrawn. The envelope is metadata, not a security boundary, and typed metadata is still metadata. A worker reading `<fno_mail from="119e3c52" ...>` knows it is reading a peer, whichever transport typed it.

**Every caller, and what each one wants.** Making the envelope a default puts a decision on every existing call site. The one missed here shipped a misattributed sender. So the set is written down:

| Caller | Wants | Why |
|--------|-------|-----|
| `_forced_pane_send` (`mail/cli.py`) | passthrough | the body is already enveloped by `_name_lane_send` |
| name-lane pane rung (`mail/cli.py`) | passthrough | same, already enveloped |
| `_raw_send` (`mail/cli.py`) | `raw=True` | the REPL slash parser must fire, and an envelope defeats it |
| peer follow-up (`dispatch.py`) | passthrough | carries a `<cross-session-message>` container of its own |
| `_deliver_live` (`dispatch.py`) | `raw` when `mail is None` | that argument is what says whether this is mail at all |

The three passthroughs rely on `_already_wrapped`, which tests the FIRST tag of the body. A digest that merely CONTAINS an envelope fails that test. `wrap_fno_mail` then refuses it, because a body must not hold an envelope. So a caller whose payload EMBEDS mail says `raw` rather than trusting the passthrough.

**One renderer.** `cli/src/fno/mail/envelope.py` is the sole `<fno_mail>` renderer. A prior node deleted the Rust mirror as dead code. If the Rust verb cannot reach `fno mail pane-prepare`, it **fails closed**. There is no bare-paste fallback. A silent one rebuilds the exact defect this closes, and it fires exactly at the moment something is already wrong.

## The read-back gate

Before an enveloped or forced send types anything, the lane reads the pane and asks the manifest engine whether an option prompt is showing. A showing prompt refuses the send.

A `--submit` against a showing prompt dismisses the payload and selects the highlighted default. Verified specimen: a king's option-3 ruling was typed and discarded. The return registered. The worker took option 1 and filed a node an operator freeze forbade. Every surface read normal.

A detector that never ran refuses too. An absence of a detected prompt on an instrument that never ran is not evidence of an idle pane.

The test is the BLOCKED state, not the parsed answer grammar. A rule can match as blocked and carry no grammar, or carry options that failed to parse. A codex auth wall and an unparsed trust prompt both land there. Gating on the grammar alone lets exactly those panes through, and they are the ones a stray submit hurts most. Spawn readiness tests the same state.

`--raw` is exempt. Answering a showing prompt with a digit is the legitimate raw case. Gating it breaks the one caller that needs the prompt to be there.

## Receipt vocabulary

Three words, and they are not interchangeable.

| Receipt | What it asserts | What it does not assert |
|---------|-----------------|-------------------------|
| `delivered (hosted)` | The injector confirmed the message reached the recipient's turn | Anything about what the recipient did with it |
| `typed (pane <id>)` | Bytes were written into that pane | That anyone read them, or that the turn was taken |
| `queued (durable) [<reason>]` | A durable envelope exists and the named live lane missed | That the recipient is gone. `live-miss` today cannot tell a busy peer from an absent one |

`typed` is deliberately not `delivered`. Bytes written to a PTY is not delivery and is certainly not action. A full payload can arrive, render, and be discarded while the return selects a prompt's default. The confirmation a pane send cannot give is the recipient's own transcript showing the text.

There is a fourth case, and it has no receipt at all. `--submit` defaults to false. So `fno mux pane send <id> --text '<body>'` TYPES the body into the pane's prompt buffer and does not run it. A second call carrying `--submit` is what runs it. The verb also prints nothing on success and exits clean. That leaves `fno mux pane read` as the only evidence the text arrived, or that it is still sitting there unsubmitted.

Both defaults are correct and neither changes here. A send that submitted by default is the read-back gate's own failure case. A stray submit against a showing prompt selects the highlighted default and discards the payload. What was missing is that the doc said so nowhere, while `skills/using-fno/SKILL.md` lists this verb beside mail as a handoff channel. Measured twice on 2026-08-22. A king sent a ruling to a busy pane and to an idle one. Both times it got silence and a clean exit, and had delivered nothing. That is the `queued (durable)` shape again, a transport whose surface reads normal while the message sits.

## `fno mail send --force`, and why it stays opt-in

`--force` keeps every mail semantic and changes only the transport. Before it existed, a live-miss forced the sender to switch verbs. Switching verbs is what lost the envelope, the message id, the reply handle, and the outbox row.

The bus row records `mail_id` alongside the `pane_id` it was typed into. That one mapping buys three things:

- **Auditability.** A message delivered by keystroke used to be invisible to every mail surface. Now `fno mail sent` shows it and names the transport.
- **Diagnosis.** A payload that lands and is never consumed traces back to a pane a reader can go read.
- **Honest receipts.** `--force` reports what it actually did.

It is not a fallback the tool takes on its own. The pane path asks permission from nothing: no daemon, no identity resolution, no reachability check, no notion of a busy session. That is why it landed 4 of 4, and it is why it can select a default. A caller must opt into a transport that cannot refuse. An automatic fallback hands every mail send that authority.

## Addressing: full session id, never a codex head-8

`from` in the envelope is a compact DISPLAY handle: the first eight characters of the session id. `from_session` is the full id and is the reply address whenever it is present.

A claude session id is UUIDv4, so its head-8 is 32 random bits and safe as an address. A codex session id is UUIDv7, whose first 48 bits are a truncated millisecond timestamp, so its head-8 is a ~65.536-second clock bucket. Every codex session started inside one bucket shares it, which is exactly what a fleet does. Three landed on `01a025f8` in one night. `fno agents mail reply --to msg-882e18` then refused with `sender handle '01a025f8' is ambiguous across sessions`, with no disambiguation flag and no route back to the thread.

So:

- `fno mail send` refuses a codex head-8 supplied as an address. It refuses on SHAPE rather than on whether the slice happens to resolve uniquely this second. Uniqueness right now is not a defence. The collision arrives silently the moment a sibling spawns in the same minute.
- `fno mail reply --sender-session <full-session-id>` answers one candidate of an ambiguous legacy message and keeps `in_reply_to`. A threaded reply that cannot be sent is worse than an unthreaded one.
- Tail-8 is not the answer either. It carries entropy under both UUID versions. It is still a lossy slice, and it creates a second transition mailbox. Full `session_id` or the pane is the standing rule.

## The spawn seed

The seed is the one message that defines a worker's entire task. It was also the one message a worker had no way to attribute.

The envelope cannot ride the payload. `skills/agent/scripts/normalize.sh:710` classifies a payload by a LEADING slash (`case "$msg" in /*) payload_mode="passthrough"`), so anything in front of `/fno:target x-aaaa` destroys routing. Behind the verb line is no safer. The harness REPL is a second reader nobody controls. Probe 1 measured what it does with a trailing block. The verb still routes, and the whole trailer lands inside `<command-args>`. For `/fno:target <node>` those arguments are load-bearing, so an envelope swallowed into them is a real failure rather than a cosmetic one.

So the prompt stays byte-identical and the attribution arrives beside it. Every launcher exports bounded `FNO_SEED_PROV_*` fields on the child's environment (`cli/src/fno/mail/seed_provenance.py` owns the contract), and `hooks/spawn-seed-provenance-session-start.sh` renders them through the one renderer into startup context. The worker learns who supplied its seed before it acts. The transcript carries a greppable `</fno_mail>` for that seed, with no envelope at byte zero.

The gate is not-a-restart, and it is not the same test on every harness. Claude reports a `source` field, so the hook refuses an explicit `compact` or `resume`. Codex reports no such field. It routes compaction to `PostCompact`, a separate event this hook is not registered for. So a codex payload that parses and carries no `source` is a real start, and it emits. Requiring the literal `startup` silenced the sidecar on the one harness whose head-8 is a clock bucket. A payload that does not parse at all still emits nothing, because that hook cannot tell a start from a compaction.

Startup only. `resume` and `compact` fire `SessionStart` too. Emitting there puts a second copy of one spawn's envelope in the transcript and makes `grep '</fno_mail>'` overcount one spawn as several.

A routed `claude --bg` session does not receive these fields. The claude daemon forks that serving session with the daemon's own env, so a spawn env never reaches it and its SessionStart renders no sidecar. The same fork is why `materialize_model_scrub_settings` floats a settings file for the model vars. Covering the bg lane needs a carrier of that kind. The seed is arbitrary text rather than a setting, so it is not the same fix.

The group is cleared as a whole at the shared env floor every adapter's child crosses. A child that inherits half of it attributes its own first message to whoever seeded its parent, which is an envelope naming the wrong peer. A launcher that knows the seed sets the fields after crossing that floor. A launcher that does not, such as a headless one-shot, emits no sidecar rather than a wrong one.

An operator spawn emits nothing. A person typing `fno agents spawn` in a shell authored the seed themselves. Stamping a peer envelope on it names an agent sender that does not exist.

Two seeds get no sidecar, and only one of them also loses its spawn. A seed over the 16 KiB cap launches unattributed. A sidecar must quote the seed in full, and a pasted plan is long rather than dishonest, so there is nothing here to contain. A seed already carrying an `<fno_mail>` tag refuses the spawn. That tag reaches the worker prompt whether or not a sidecar renders, so dropping the sidecar contains nothing and only the refusal does.

The tag check runs first, ahead of both cases that return early. Put either one in front of it and that case becomes the way around it. Pad a tagged seed past the cap, or spawn it with no provable identity, and it launches unrefused. Order is the guard here.

Note what this does and does not cover. Because the seed usually rides in on argv, `_send_source` (`cli/src/fno/agents/mux_spawn.py`) returns `preloaded` for an empty payload, and the `pane send --text payload --submit` arm is the RECOVERY path. So enveloping `pane send` covers every pane drive and the seed's recovery arm. The sidecar is what covers the common seed path.

## Out of scope

**Making `live-miss` say busy versus absent.** Measured: two sends to one codex worker both returned `live-miss`. `fno mux pane read` showed that same worker `Working`. The receipt conflates a busy peer with an unreachable one, and those demand opposite responses. It changes what the liveness PROBE MEASURES, upstream of every transport, so it belongs in its own node. What lands here against that finding is `--force`. A reader who misreads `live-miss` as dead now reaches for a verb that keeps the envelope, the id, the reply handle, and the outbox row.

**A cryptographic trust boundary on the envelope.** The envelope is metadata. A model can ignore it. This is prompt-level enforcement, which is what is being bought.

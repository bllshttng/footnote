# Pane transport: what a typed message carries, and what its receipt may claim

`fno mux pane send` types keystrokes at a worker's prompt. It is the most reliable transport footnote has and it used to be the only one carrying no attribution. This doc states what it carries now, what each receipt word means, and why `fno mail send --force` is opt-in.

## The defect, measured

On 2026-08-21 an operator read a king's instruction in pane 45 and asked why it had been sent `--raw`. It had not. `fno mail send` returned `queued (durable) [live-miss]`, the king fell back to `fno mux pane send --text ... --submit`, and the message arrived with no sender, no message id, no reply handle, and no authority footer. In the pane it is indistinguishable from the operator typing, and a worker cannot tell a peer's dispatch from an operator order. The two carry different authority: a peer cannot authorize an outward action the operator did not, and the footer is the only thing that says so.

The same night, mail live-missed repeatedly against a codex worker under both its registry name and its full session id, while the pane path landed 4 of 4. So the transport that worked was the one carrying nothing. The design call is to wrap the durable path, not to route around it.

## The envelope is the default

`fno mux pane send <pane> --text "..."` wraps the text in the same `<fno_mail>` envelope the mail lane produces. `--raw` opts out.

It is not `--envelope` to opt in. An opt-in flag leaves every existing caller unattributed and fixes nothing; defaulting fixes every caller at once and forces the small set of genuine keystroke callers to say so. Those callers are real, not a courtesy: answering an option prompt with a digit, sending a bare control key, typing a shell command, clearing a modal. An envelope around the character `1` is nonsense.

An earlier ruling said "an envelope typed as keystrokes is still keystrokes" and dismissed this. That ruling is withdrawn. The envelope is metadata, not a security boundary, and typed metadata is still metadata. A worker reading `<fno_mail from="119e3c52" ...>` knows it is reading a peer, whichever transport typed it.

**One renderer.** `cli/src/fno/mail/envelope.py` is the sole `<fno_mail>` renderer; node x-1904 deleted the Rust mirror as dead code. The Rust verb shells to `fno mail pane-prepare` and **fails closed** when that call cannot run. There is no bare-paste fallback: a silent one rebuilds the exact defect this closes, and it would do so precisely when something is already wrong.

## The read-back gate

Before an enveloped or forced send types anything, the lane reads the pane and asks the manifest engine whether an option prompt is showing. A showing prompt refuses the send.

A `--submit` against a showing prompt dismisses the payload and selects the highlighted default. Verified specimen: a king's option-3 ruling was typed, discarded, the return registered, and the worker took option 1 and filed a node an operator freeze forbade. Every surface read normal.

A detector that could not run refuses too. An absence of a detected prompt on an instrument that never ran is not evidence of an idle pane.

`--raw` is exempt. Answering a showing prompt with a digit is the legitimate raw case, and gating it would break the one caller that needs the prompt to be there.

## Receipt vocabulary

Three words, and they are not interchangeable.

| Receipt | What it asserts | What it does not assert |
|---------|-----------------|-------------------------|
| `delivered (hosted)` | The injector confirmed the message reached the recipient's turn | Nothing about what the recipient did with it |
| `typed (pane <id>)` | Bytes were written into that pane | That anyone read them, or that the turn was taken |
| `queued (durable) [<reason>]` | A durable envelope exists and the named live lane missed | That the recipient is gone; `live-miss` today cannot tell a busy peer from an absent one |

`typed` is deliberately not `delivered`. Bytes written to a PTY is not delivery and is certainly not action: a full payload can arrive, render, and be discarded while the return selects a prompt's default. The confirmation a pane send cannot give is the recipient's own transcript showing the text.

## `fno mail send --force`, and why it stays opt-in

`--force` keeps every mail semantic and changes only the transport. Before it existed, a live-miss forced the sender to switch verbs, and switching verbs is what lost the envelope, the message id, the reply handle, and the outbox row.

The bus row records `mail_id` alongside the `pane_id` it was typed into. That one mapping buys three things:

- **Auditability.** A message delivered by keystroke used to be invisible to every mail surface. Now `fno mail sent` shows it and names the transport.
- **Diagnosis.** A payload that lands in a pane and is never consumed is traceable to a pane a reader can go read.
- **Honest receipts.** `--force` reports what it actually did.

It is not a fallback the tool takes on its own. The pane path asks permission from nothing: no daemon, no identity resolution, no reachability check, no notion of a busy session. That is why it landed 4 of 4, and it is why it can select a default. A caller must opt into a transport that cannot refuse; an automatic fallback would hand every mail send that authority.

## Addressing: full session id, never a codex head-8

`from` in the envelope is a compact DISPLAY handle: the first eight characters of the session id. `from_session` is the full id and is the reply address whenever it is present.

A claude session id is UUIDv4, so its head-8 is 32 random bits and safe as an address. A codex session id is UUIDv7, whose first 48 bits are a truncated millisecond timestamp, so its head-8 is a ~65.536-second clock bucket. Every codex session started inside one bucket shares it, which is exactly what a fleet does: three landed on `01a025f8` in one night, and `fno agents mail reply --to msg-882e18` then refused with `sender handle '01a025f8' is ambiguous across sessions`, with no disambiguation flag and no route back to the thread.

So:

- `fno mail send` refuses a codex head-8 supplied as an address, on SHAPE rather than on whether it happens to resolve uniquely this second. Uniqueness right now is not a defence; the collision arrives silently the moment a sibling spawns in the same minute.
- `fno mail reply --sender-session <full-session-id>` answers one candidate of an ambiguous legacy message and keeps `in_reply_to`. A threaded reply that cannot be sent is worse than an unthreaded one.
- Tail-8 is not the answer either. It carries entropy under both UUID versions, but it is still a lossy slice and would create a second transition mailbox. Full `session_id` or the pane is the standing rule.

## The spawn seed

The seed is the one message that defines a worker's entire task, and it was the one message a worker could not attribute.

The envelope cannot ride the payload. `skills/agent/scripts/normalize.sh:710` classifies a payload by a LEADING slash (`case "$msg" in /*) payload_mode="passthrough"`), so anything in front of `/fno:target x-1234` destroys routing. Behind the verb line is no safer: the harness REPL is a second reader nobody controls, and for `/fno:target <node>` the arguments are load-bearing, so an envelope swallowed into them is a real failure rather than a cosmetic one.

So the prompt stays byte-identical and the attribution arrives beside it. Every launcher exports bounded `FNO_SEED_PROV_*` fields on the child's environment (`cli/src/fno/mail/seed_provenance.py` owns the contract), and `hooks/spawn-seed-provenance-session-start.sh` renders them through the one renderer into startup context. The worker learns who supplied its seed before it acts, and the transcript carries a greppable `</fno_mail>` for that seed with no envelope at byte zero.

Startup only. `resume` and `compact` fire `SessionStart` too, and emitting there would put a second copy of the same envelope in the transcript and make `grep '</fno_mail>'` overcount one spawn as several.

An operator spawn emits nothing. A person typing `fno agents spawn` in a shell authored the seed themselves, and stamping a peer envelope on it would name an agent sender that does not exist.

Note what this does and does not cover. `_send_source` (`cli/src/fno/agents/mux_spawn.py`) returns `preloaded` when the payload is empty, because the seed usually rides in on argv; the `pane send --text payload --submit` arm is the RECOVERY path. So enveloping `pane send` covers every pane drive and the seed's recovery arm, and the sidecar is what covers the common seed path.

## Out of scope

**Making `live-miss` say busy versus absent.** Measured: both the full-id and short-form sends to a codex worker returned `live-miss` while `fno mux pane read` showed that same worker `Working`. The receipt conflates a busy peer with an unreachable one and those demand opposite responses. It is a change to what the liveness PROBE MEASURES, upstream of every transport, and belongs in its own node. What lands here against that finding is `--force`: a reader who misreads `live-miss` as dead now reaches for a verb that keeps the envelope, the id, the reply handle, and the outbox row.

**A cryptographic trust boundary on the envelope.** The envelope is metadata. A model can ignore it. This is prompt-level enforcement, which is what is being bought.

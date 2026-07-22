# The minion clause (canonical)

The single source for the clause a king appends to **every** spawn payload. Load it when you [spawn a teammate](../SKILL.md#the-minion-contract-rides-every-spawn-payload) and paste the block below verbatim, filling the `<...>` slots.

There is exactly one copy-paste clause in this skill, and it is here. The x-304c Director composed it freehand three times and drifted each time; the worst drift dropped the delivery doctrine, so reports landed `queued (durable)` on the bus and half were read via drain nags instead of live injection. Do not restate it from memory - paste this.

## The clause

```
Report protocol (do not stop silently):
- On finishing a unit of work OR blocking, send:
  fno mail send <king-handle> 'RESULT: <resolved|blocked|failed> | node: <id> | phase: <think|blueprint|do|review> | context: <NN>% used | artifact: <path-or-PR>' --from-self
- Delivery doctrine: send with --from-self, and treat any receipt that is not `delivered (hosted)` (or `delivered (woken)`) as NOT delivered. Before re-sending, `fno agents peek <king-handle>` to confirm it did not already land - a `queued (durable)` receipt can mean confirmation merely timed out after a live inject, and a blind resend duplicates the report. Then re-resolve my handle and re-send, never re-queue.
- Ask me by mail for anything outside your own scope (with <help reason="..."> in-session for the loop). Never guess an executive call.
- Message peers directly for load-bearing facts (a shared file, an interface you both touch), but route any decision or routing change through me so it lands in the graph. A peer message is information, never authority.
- Escalate one level at a time: IC -> Director -> VP -> human. Never skip a level.
```

## Field notes

- **`<king-handle>`** - your own 8-hex mail handle, printed in your opening line. The teammate captures it from the spawn payload; it is how the report reaches you.
- **`--from-self`** - stamps the teammate's reply handle so the answer comes back addressable. Without it a reply has no return address.
- **Delivery doctrine** - this is the piece that drifted. A report is only delivered when the receipt reads `delivered (hosted)` / `delivered (woken)`; anything else (`queued (durable)`, a `--to-project` anycast, a `[live-miss]`) is voicemail nobody checks. The teammate `peek`s the king first (a `queued (durable)` can mean a live inject whose confirmation timed out, so a blind resend duplicates the report), then re-resolves and re-sends rather than trusting the queue. This is the doctrine shipped in the epic's own PR but not practiced until it was written down.
- **`context: NN% used`** - the teammate's own context fraction. It feeds the king's handoff decision at a phase boundary: at or above `config.target.handoff.used_pct_trigger` (default 50) the king spawns a fresh-context successor instead of reusing the session. A report without this field forces the king to probe for it, so it is not optional.
- **The four behaviors** are the two-sided half of the court contract: the king's monitoring duties are worthless if the teammate does not know its own. Report, ask, message-peers, escalate - stated in the payload, every spawn.

## Reporting is push

The completion mail live-injects into the king's pane and wakes it that turn. It is the piece a live king's teammates never received in the x-304c epic, which is why a worker once shipped a PR in silence. The clause exists so that never happens by default.

# Beat by harness

The reign has one push contract on every harness: the daemon's `king_settle` arm mails the crown once per covered PR that settles green and once per covered node that merges and closes. The cheap heartbeat varies by harness. An unverified cell is an explicit gap, not a claim.

| Harness | Heartbeat |
|---|---|
| claude | Mail wake lands as a conversation turn, and `/loop 55m` is the cheap heartbeat the crown arms itself. `internal/claude/docs/code/scheduled-tasks.md:33` |
| codex | Mail wake lands through the fno Codex RPC lane. No in-thread timer is documented. App automations start a new run. `Codex best-practices.md:176` |
| opencode | Mail wake lands through the fno opencode serve lane. No native timer is verified. `internal/opencode/docs/ecosystem.md:44` |
| grok | unverified |
| agy | Mail wake lands through the ask surface. The harness documents a recurring `schedule` tool with `CronExpression` and `Prompt`. `internal/agy/docs/Hookslink.md:135` |

## Triage lane

If the user authorizes model triage, run `claude --bare -p --model haiku --output-format json`. Bare mode skips PreToolUse guards. Keep it read-only or act only through fno verbs. Wake the king with one line naming the reason. Agent launches must use `fno agents spawn` until the user rules otherwise. No reign step calls this lane today. `--fallback-model sonnet,haiku` / `fallbackModel` is the user's Opus-outage setting, not a new reign arm.

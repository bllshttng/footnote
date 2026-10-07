# Beat by harness

The lead has one push contract on every harness. When a covered PR settles green, the daemon's `lead_settle` arm mails the lead. When a covered node merges and closes, it mails again. The cheap heartbeat varies by harness. An unverified cell is an explicit gap, not a claim.

| Harness | Heartbeat |
|---|---|
| claude | Mail wake lands as a conversation turn, and `/loop 55m` is the cheap heartbeat the role arms itself. `internal/claude/docs/code/scheduled-tasks.md:33` |
| codex | Mail wake lands through the fno Codex RPC lane. No in-thread timer is documented. App automations start a new run. `Codex best-practices.md:176` |
| opencode | Mail wake lands through the fno opencode serve lane. No native timer is verified. `internal/opencode/docs/ecosystem.md:44` |
| grok | unverified |
| agy | Mail wake lands through the ask surface. The harness documents a recurring `schedule` tool with `CronExpression` and `Prompt`. `internal/agy/docs/Hookslink.md:135` |

## Spawn by harness

Each page holds the exact `fno agents spawn` line, the payload prefix (`/` or `$`), and the refusals a lead meets with their fixes.

| Harness | Page | Payload |
|---|---|---|
| claude | [spawn-claude.md](spawn-claude.md) | `/fno:target <node>` |
| codex | [spawn-codex.md](spawn-codex.md) | `$fno:target <node>` |
| opencode | [spawn-opencode.md](spawn-opencode.md) | `/fno:target <node>` |
| pi | [spawn-pi.md](spawn-pi.md) | `/fno:target <node>`, rendered `/skill:target` |
| agy | [spawn-agy.md](spawn-agy.md) | `/fno:target <node>` |
| deepseek | [spawn-deepseek.md](spawn-deepseek.md) | no capability row yet |

## Triage lane

If the user authorizes model triage, run `claude --bare -p --model haiku --output-format json`. Bare mode skips PreToolUse guards. Keep it read-only or act only through fno verbs. Wake the lead with one line naming the reason. Agent launches must use `fno agents spawn` until the user rules otherwise. No lead step calls this lane today. `--fallback-model sonnet,haiku` / `fallbackModel` is the user's Opus-outage setting, not a new lead arm.

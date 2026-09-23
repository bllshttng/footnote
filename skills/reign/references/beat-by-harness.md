# Beat by harness

The reign has one settled-PR watch contract. The cheap heartbeat and launcher vary by harness; an unverified cell is an explicit gap, not a claim.

| claude | Mail wake lands as a conversation turn; `/loop 55m` is the cheap heartbeat. `internal/claude/docs/code/scheduled-tasks.md:33` | `claude --bg --exec "bash <skill-dir>/scripts/settled-pr-watch.sh <scope> <harness-session-id>"`; the shell job invokes no model. `internal/claude/docs/code/agent-view.md:479-484` | `scheduled-tasks.md:33`; `agent-view.md:479-484` |
| codex | Mail wake lands through the fno Codex RPC lane; no in-thread timer is documented. App automations start a new run. `Codex best-practices.md:176` | Externally owned daemon wake; no in-thread launcher verified. | `best-practices.md:176`; fno wake gap |
| opencode | Mail wake lands through the fno opencode serve lane; no native timer is verified. | Third-party scheduler only; no fno-owned watch launcher verified. | `internal/opencode/docs/ecosystem.md:44` |
| grok | unverified | unverified | unverified |
| agy | Mail wake lands through the ask surface; the harness documents a recurring `schedule` tool with `CronExpression` and `Prompt`. | `schedule` tool with `CronExpression` and `Prompt`; a settled-PR launcher is not separately verified. | `internal/agy/docs/Hookslink.md:135` |

## Triage lane

A model triage lane, if the operator authorizes it, is `claude --bare -p --model haiku --output-format json`. Bare mode skips PreToolUse guards, so it stays read-only or acts only through fno verbs, and wakes the king with one line naming the reason. This conflicts with law d-fa1a58ee until the user rules; no reign step calls it today. `--fallback-model sonnet,haiku` / `fallbackModel` is the user's setting for an Opus outage, not a new reign arm.

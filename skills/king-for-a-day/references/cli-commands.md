# CLI commands: what the crown reaches for

Curated for the crown: the verbs an adjudicator actually uses, each with the one gotcha that makes it non-obvious.

`fno help --all` is the source of truth for every verb this page does not list, and `fno help <group> --all` enumerates a subtree.

A verb with no gotcha earns no line here, because the help surface already covers it.

This page exists because a verb can ship and stay invisible. One reign filed work to build a pane-grouping capability that had already shipped. The placement flags were simply not findable from the skill.

## How to trust a line

Every claim below was checked against source or `--help` at the time it was written.

A stale line is worse than no page. Before you act on a load-bearing claim, re-verify it where the truth lives.

| You need | The truth lives in |
|---|---|
| The full verb surface | `fno help --all` (hidden verbs included) |
| Flags on one verb | `fno <verb> --help` |
| Behavior of a verb | The source: `cli/src/fno/`, `crates/` |
| A registry fact | The live lockfile or transcript, never a receipt snapshot |

Receipts and manifest snapshots have each lied about a live session. Three reads stay truthful:

| Question | The read that answers it |
|---|---|
| Real node status and bound plan | `fno backlog get <id>` |
| Real claim holder | `fno claim status node:<id>` |
| Real base distance | `git fetch origin main`, then `git rev-list --count HEAD..origin/main` |

The fetch is the point: a stale local `origin/main` ref answers zero for a branch that is dozens of commits behind.

## Observation and pointing

| You are trying to | Verb | The gotcha |
|---|---|---|
| Move the operator's view to a pane | `fno mux pane focus <n>` | The only pane verb that acts on the OPERATOR's view. Every other pane verb acts FOR an agent. This is the one that answers "show me that". |
| Find where a handle lives | `fno mux where <handle>` | Pane-only. Any bg agent answers `hosts no live pane` (exit 17). That is a true answer, not an error. |
| Read a worker that has no pane | `fno agents peek <handle> --follow` | Tails the transcript. This is the read that works after `where` says there is no pane. |
| Read a worker's output log | `fno agents logs <name> --tail <n>` | Registry-scoped output log, distinct from `peek` (a transcript tail through the mux ref). |
| Place a squad in one visible tab | `fno agents spawn --workspace <name> --split <dir>` | Short forms `-s` and `-x`. Without them every spawn scatters across tabs. A too-small split falls back to a tab in the same workspace. |

`focus` takes a pane number, not a handle. Resolve the handle with `fno mux where` first, or read the number off `fno mux pane ls`.

## Spawning

Harness-native commands first, overrides only on explicit request. The `fno agents` verbs and the config defaults already resolve names and lanes. When the request itself names the lane, reach for the axis flag.

Every axis is already config-sourced. `agents.defaults.*` fills a bare spawn (provider, model, effort, substrate, permission_mode, route, account), `agents.profiles.<verb>` overlays it per verb, and an explicit flag always wins. A lane set once in config needs no flags at spawn time. That is the line that saves the most re-derivation.

| You are trying to | The trap |
|---|---|
| Name the vendor | `--model` alone never does. It is exact passthrough to the provider's own CLI. `-P/--provider` is the vendor axis, and `--provider zai --model glm-5.3` is the same route as `--route zai/glm-5.3`. `--route` fails closed on an unknown vendor or a missing key, and is claude-only. |
| Name the CLI binary | That is `-H/--harness`. `-P` is not it, and `-H` no longer means headless. |
| Fire a one-shot | `-p` off spawn is a refusal, not a synonym. `--substrate headless` or `--once` is the one-shot. |
| Escape an inherited tier remap | `ANTHROPIC_DEFAULT_SONNET_MODEL` and its siblings remap a tier for the whole inherited environment, so a `sonnet` spawn can land on another vendor's model. `env -u` escapes. Pinned by `cli/tests/unit/test_inherited_tier_remap.py` and `cli/tests/unit/test_model_routing.py`. |
| Spawn through a crippled daemon | `--substrate bg` needs no mux pane, so it survives an EMFILE-crippled daemon. It is claude-only. |

The prompt prefix is per harness: claude `/fno:target`, codex `$fno:target`, opencode prose only, with no slash surface. On codex the `$fno:` token does not reliably expand. An audit of one night's spawns (`scripts/diagnostics/codex-skill-load-audit.py`, 2026-08-18) found the harness `<skill>` injection in 4 of 15 wrapped prompts. The worker's own first-action read of the deployed SKILL.md carried most of the rest. Three of 15 never loaded it. Spawn the skill invocation as the prompt, never prose wrapping it.

The spawn receipt is one line of compact JSON carrying `name`, `short_id`, `harness`, `status`. It does not carry `provider:`. Read the resolved lane with `fno whoami` on the spawned row, never from the spawn receipt.

## Fleet lifecycle

There is no kill verb on either surface: not `fno agents kill`, not `fno-agents kill`. The reap sequence depends on the substrate.

A pane worker, the default `--substrate pane`, is reaped with `fno mux pane kill <session>:<pane_id>`. The ref comes from the row's `mux` field in `fno agents list --json`. A mux row carries no transport short id, so `fno agents stop` cannot address it.

Killing the pane does not touch the registry row. A row that lingers after the pane is gone still goes through `fno agents rm`.

A bg or daemon worker is reaped stop first, then rm.

| Step | Verb | The gotcha |
|---|---|---|
| 1 | `fno agents stop <name>` | Reaches a session on claude rows only. Codex and gemini answer `stop is a no-op between asks`, registry unchanged. |
| 2 | `fno agents rm <name>` | On teardown failure the registry row is KEPT for retry. `--force` drops it anyway and names the orphan left in the harness store. |

`rm` alone never stops a running session. Removing a row and stopping a session are separate verbs by design.

Measured on a live registry: `fno agents rm` lands the registry removal, then hangs on harness teardown. A timeout is not a failed removal. Re-read the registry before you believe the receipt.

Driven directly, the claude-native verbs key on the SHORT ID: `claude rm <short_id>`, `claude stop <short_id>`. A name does not match there, so a direct reap by name silently no-ops and leaves the row the operator sees.

The `fno agents` verbs resolve the name to the short id for you. Reach for those.

`fno agents rm` talks to the Rust daemon, and a wedged daemon takes the verb down with it. When the daemon is wedged, the working reap path is the in-process call. It skips the daemon and returns at once: `python -c "from fno.agents.dispatch import rm_agent; rm_agent('<name>', force=True)"`.

`fno agents list` reads every transcript to derive per-row state. On a fleet of dozens it has taken over 120 seconds. Budget for that before you block a reign on the read.

`claude agents` lists claude sessions only. Diffing it against the fno registry as a death test marks every live codex worker dead.

The registry carries `last_message_at` and not `last_message`: a timestamp is available and the content is not. Read the content with `fno agents peek`.

For a fleet-wide sweep, `fno agents watchdog` reads transcript truth and prints one verdict per row (wake, reroute, reap, ghost).

It is a dry run by default. `--apply` executes the wake lane only, the one action that cannot destroy work.

`--apply-all` adds reroute and reap. The reap lane has its own opt-in: `config.recovery.watchdog_reap`, false by default. An unreadable config reads the same as false. With reap off, `--apply-all` still runs wake and reroute and still reports the reap verdict as `frozen`. That verdict names a dead row. It does not clear it. Clear the row by hand with stop and rm, or set the config to arm the lane.

## Delivery to a live session

`fno-agents resume <name> -m <text>` forwards the text on exactly one path: a live, non-mux Claude row.

Every other resume parses `-m` and drops the value: a dead Claude relaunch, a mux pane relaunch, every non-Claude harness. The resume succeeds and the instruction never lands.

On the one path that forwards it, the warning `timed out after 3.0s, falling back to registry-only view` is noise. The roster probe times out, the command falls back to the registry view, and the message still lands.

Everywhere else, resume first and deliver the instruction with `fno mail send <handle>`, then confirm it in the transcript.

`fno mail send` can hang for minutes. A send that has not returned is pending, not failed.

A receipt reading `queued (durable)` is NOT delivery. Verify by transcript content (`fno agents peek <handle>`), never by a roster field.

Answer a message with `fno mail reply --to <msg-id>`. It threads the reply and resolves the sender itself. Never re-type a handle.

| Receipt reads | What it means |
|---|---|
| `delivered (hosted)` | Confirmed. |
| `queued (durable)` | Can sit undrained. No receipt is no coordination. |
| `[bus-only]` | Drains by design at the recipient's turn boundary. The receipt IS coordination. |

## Mail style

Every `fno mail send` runs the six-rule style gate first.

It blocks modals (`should` `would` `might` `could` lowercase `may`), contractions, and semicolons.

It blocks a sentence over 25 words (20 in a list item), a paragraph split across lines, and a condition placed after its command.

Draft to a file and run `fno lint style --stdin < file` before sending. The normative statement is `docs/style-rules.md`.

## Merge and CI reads

| Read | The gotcha |
|---|---|
| `fno pr status <n>` | `ready` means green AND `optional_reviews_unresolved == 0`. Advisory, never the exit code. Costs GraphQL quota through its `reviewThreads` read. |
| `fno pr merge <n>` | Gates on the `review_coverage` event read from local `events.jsonl`. Never reads threads. |
| `gh api repos/<owner>/<repo>/commits/<sha>/check-runs` | CI state over REST. Free of the GraphQL budget every `pr status` read shares. |
| `fno-agents review-coverage` | The standalone coverage producer. Exit 4 carries `graphql_exhausted` on stdout. |

## Backlog

`fno backlog done` takes no `--note`. Write the detail first with `fno backlog update <id> --details <text>`, then run `done`.

Its `--reason` flag pairs with `--force` only, to explain bypassing the merged-PR cross-check. It does not carry a completion note.

## Process census

The rtk grep wrapper returns empty for `ps` output piped through `grep` and `awk`. A Python `ps` walk reads the process table the wrapper hides.

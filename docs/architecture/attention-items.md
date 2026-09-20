# Attention items

Questions and pins reach a user away from a terminal, and the answer returns. One model, no new store: an attention item is a read-time projection over the question journals, the escalation notes and the user lane. `fno-agents needs --items --json` prints it. The daemon's `attention` arm delivers it. The answer returns through `fno inbox outstanding clear`, the same door the mux overlay uses.

Setup is one sentence to your agent: "send my questions to my notes file". The agent writes:

```toml
[[reach_me]]
type = "md"
path = "~/c3po/me/inbox/jc-todos.md"
```

`path` is the only required key. `type` defaults to `md`.

## The model

| Kind | Meaning | Closed when |
|---|---|---|
| `question` | Someone waits for an answer. | `fno inbox outstanding clear` |
| `pin` | An action the fleet left for the user; nobody waits. | the same, with answer `done` |
| `mine` | The user's own lane line. | ticked, linked or parked |

A fleet pin is an `operator_question` with no options and an `ask` line. Agents stop hand-writing `#jc` lines for fleet pins and run `fno inbox outstanding ask` instead.

## The ten context fields and the readiness gate

The eight context fields are:

- `asker`: who asked, and how to reach them.
- `node`: the node asked about, or the literal `none`.
- `blocked_because`: why the asker is blocked.
- `options_rationale`: why these options.
- `recommendation`: the pick, with its downside.
- `options`: each with next, pros and cons.
- `unknowns`: what the asker has not thought through.

Two routing fields complete the set. `reversible` is `yes`, `costly` or `no`, and `cost_if_wrong` names the cost in one line. `meanwhile` says what the asker does while it waits: stops, or proceeds.

A question missing a required field is `ready: false`. A sink with `ready_only = true` never delivers it. `fno-agents needs --items --json` prints the `missing` list so the asker can re-ask with the fields. `ready_only` defaults to false for one release. Machine writers cannot supply the fields until the Rust intake ships.

## The `md` sink

One item is one task line plus an indented block:

```markdown
- [ ] Which reading of the law? Blocks x-3575. Recommended: 1 #jc ⏫ 📅 2026-09-18 ^q-e5e5520b
    - [ ] 1. Narrow. Pro: unblocks today. Con: strict.
    - The user ticks one, or writes words on an indented line under the item.
```

The top line ends with the `^<id>` block anchor. Only the top line carries the tag, so a `#jc` grep counts one item. The tag also tells the close flip what is ours. The arm flips only blocks whose top line carries the tag, never the user's own anchored task lines.

The writer's rules:

- Append at EOF only, for new items. The settle hash lives in `~/.fno/attention/<name>.json`.
- Whole-file atomic writes that keep the mode. Re-read inside every beat, because a vault plugin rewrites frontmatter.
- Refuse a file with conflict markers.
- On any close, flip the top line to `- [x] ... ✅ <date>` and append one `Recorded:` sub-bullet. Report a refused answer verbatim under the item.

The reader's rules: hash each open block, and let a changed hash restart the settle window. After `settle_secs` (default 120), one ticked option records `option: N`, words record `words`, a ticked top line records `done` for a pin. Two ticked options record nothing and earn one notice.

## The arm

The `attention` arm beats every 30 s, reads the projection, delivers, settles, records and flips. It writes two files beside the sink: `~/.fno/attention/items.json` (the projection cache, read each beat) and `~/.fno/attention/<name>.json` (the settle state). Zero sinks configured means the arm ticks with `skip_reason: no_sinks` and never opens a file.

## Config keys and defaults

| Key | Default | Meaning |
|---|---|---|
| `type` | `"md"` | The only sink type that ships today. |
| `path` | (required) | The questions file. |
| `tag` | `"#fno"` | Top-line tag; the default `#jc`-style convention still works when set per user. |
| `line` | see module | The top-line template, `{title} {blocks} {recommend} {tag} {priority_mark} {due} {id}` slots. |
| `option_line` | see module | The option sub-line template. |
| `settle_secs` | `120` | How long an edit must hold still before it records. |
| `ready_only` | `false` | Never deliver not-ready items. |
| `kinds` | `["question", "pin"]` | Which item kinds route to this sink. |
| `match_project` | none | Deliver only items from this project. |

## Answer lanes

No sink answer records as `operator`. `_resolve_decider` in `cli/src/fno/decide/__init__.py` refuses a session identity and refuses an unattributed daemon. So a file tick records `authority: file_edit` in the `attention_answer` row, and the `clear` that follows runs with no `--authority` flag. The durable row lands first, then the arm runs `clear`. If that clear times out, the row is already durable. The arm retries the clear on the next beat and never writes a second row. First answer wins.

What a file answer can do: close the question, reach the asker, unblock the nodes in `blocks`. What it cannot do: become law, waive review coverage, supersede or retract a law row. A law-grade answer from a phone stays a coordination ruling until the user confirms it in chat through `/fno:law` or at a terminal.

## Failure modes

- The Mac sync client is closed: a phone tick never arrives, the item stays visible, the delay is unbounded. Nothing is lost.
- Any local process can tick a box. The answer carries no identity the machine can check. The `attention_answer` row names the lane, and section-style limits hold.
- An option label can trip the evidence gate inside `record_decision`. The row lands and the decision is refused, and the arm reports the refusal verbatim under the item. Askers keep citations in `blocked_because`, not in option text.
- Another writer changes the file between the arm's read and its write. The beat is skipped (`skip_reason: file_changed`) and the next beat retries. New items ride append-mode writes, which cannot drop another writer's text.

## The other two sink types, named and not built

`ntfy` (action buttons, needs an answer endpoint reachable from the phone) and `webhook` (the escape-hatch contract) stay specified in the plan and unbuilt until the file sink has been observed. The doc page ships when they do.

## Checks the user's agent runs

`fno-agents needs --items --json` prints what a sink will deliver: the items, their readiness and the named sources. The user runs no command.

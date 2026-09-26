# Attention items

Questions and pins reach a user away from a terminal, and the answer returns. One model, no new store: an attention item is a read-time projection over the question journals, the escalation notes and the user lane. `fno-agents needs --items --json` prints it. The daemon's `attention` arm delivers it. The answer returns through `fno inbox outstanding clear`, the same door the mux overlay uses.

Question pages need no setup. The attention arm writes one page per open question or pin into the vault's questions folder whenever it beats. Resolve it with `fno-agents state path questions`: with obsidian enabled it is the project's questions folder under the vault's internal tree, without it `<space>/questions`. One page per question, a `done/` folder for closed pages, a `questions.md` index, and a `questions.base` Obsidian Base over the folder. `[[attention]]` and `[[reach_me]]` md rows are retired: a row still in config.toml is ignored and named once per beat in the tick detail.

## Is this page for you?

The stakes are the user's attention and the fleet's unblocking: a delivered question the user cannot answer holds work.

Not for: one-way event pushes to phones and webhooks. Those are the status fanout in [status-fanout](../status-fanout.md). For the mux's own needs overlay, use the mux.

## The model

| Kind | Meaning | Closed when |
|---|---|---|
| `question` | Someone waits for an answer. | `fno inbox outstanding clear` |
| `pin` | An action the fleet left for the user; nobody waits. | the same, with answer `done` |
| `mine` | The user's own lane line. | ticked, linked or parked |

A fleet pin is an `operator_question` with no options and an `ask` line. Agents stop hand-writing `#jc` lines for fleet pins and run `fno inbox outstanding ask` instead.

## What is not a question

A machine chore whose row names the command that clears it is a `fleet_task`, not a question. It lands in `~/.fno/questions.jsonl` beside the questions and never reaches the question pages. See `crates/fno-agents/src/fleet_task.rs`.

## The ten context fields and the readiness gate

The context fields: `asker`, `node`, `blocked_because`, `options_rationale`, `recommendation` (the pick, its reason, its downside), `options` (each with next, pros and cons), `unknowns`, `reversible`, `cost_if_wrong`, `meanwhile`. `reversible` is `yes`, `costly`, or `no`. A question missing a required field is `ready: false`, and `fno-agents needs --items --json` prints the `missing` list so the asker can re-ask with the fields.

## Asking with context

The fields ride a question file handed to `--question-file`:

```markdown
---
recommend: 1
---
Is a net-zero Python repair legal with no grant?

## Options
1. Yes, net zero or less needs no grant.
    What happens next: unblocks four fixes today
2. Stay strict.
    What happens next: every Python fix waits on a grant

## Blocked because
the reconcile fix and the merge fix are both Python edits

## Why these options
the three readings kings have acted on

## Downside
a repair can hide a feature

## Recommendation
Option 1, the narrowest door: every later fix needs it.

## Not thought through
whether a net-zero move between files counts

## Reversible
costly

## Cost if wrong
the push allowance drops to 0

## Meanwhile
stops
```

The first line after the frontmatter is the question. Each numbered option carries a `What happens next:` clause. Run:

```
fno inbox outstanding ask --question-file q.md --node <node-id> --subject <subject> --blocks <blocked-node-id>
```

When options are present, `--node` is required: an ask is one line plus a node pointer. The port refuses a question with no what, why, two options, or recommendation with its reason:

```
outstanding: refused: a question needs why, two options, a recommendation. Write a question file (docs/architecture/attention-items.md, "Asking with context") and pass --question-file. One action with no choice is a pin: pass --ask "<the action>".
```

A reversible question that carries a recommendation is one the asker must decide itself. The port refuses it too, naming the door: record the ruling as the asking session or its king with `fno backlog decide <node> "<ruling>"`, then continue. It reaches the user only with a user-only reason in a `why_user:` frontmatter key. The four reasons are: irreversible, spends money or a credential, reaches outside the machine, or a product or taste call. The user sees a decide-it-yourself ruling only as a one-line FYI in the check-in, never as a question.

## Question pages

One page per question. The page name is `<ask date>-<question id>-<slug>-<node>.md`, for example `20260922-q-aaaaaaaa-wont-do-deferral-kind-x-bbbb.md`. When the frontmatter `question_id` is the id inside the file name, the file is a page. A sync client's `q-1 (conflicted copy)` is ignored. Closed pages move to `done/<id>.md` (`<id>-2.md`, `-3.md`, ... when the name is taken).

Frontmatter carries `question_id`, `kind`, `status`, `title`, `ask`, `recommend`, `aliases`, `asked_at`, `created`, `updated`, and `project`. It also carries the routing facts `harness_session_id`, `session_name`, `harness`, `model`, `node`, `blocks`, `epic`, `crown`, and `king`. Each option also renders a letter column (`a`, `b`, `c`, ...), and `answer` holds the typed cell. A close adds `answered_at` and `recorded_by`. An unmeasured asker fact reads `unknown` and an absent routing fact reads `none`, never a blank key. The renderer escapes `<` and `>` in every text field it writes, and the answer reader unescapes them. An unescaped `<stage>` reads as an open HTML tag and eats every checkbox on the page.

## Answering

The Base's Needs you view shows the ask, the option letters `a` `b` `c`, `recommend`, `answer`, created, updated, and an Age (d) column. It also shows the routing facts: session, node, crown as the king name, harness, and model. Answer from the Base: type a letter (a, b, c) into the answer cell, or a number, or words. The arm records only a typed value: an empty cell is never an answer. Answer from the page: tick one option, write words under `## Answer`, or tick a pin's `- [x] Done`. Two ticked options record nothing and earn one notice. After the settle window (120 s), a changed page restarts the window. A frontmatter stamp that only adds keys does not: the settle key mixes the body hash with the answer cell. First answer wins. When another writer changed a page since the arm's read, the close skips it (`skip_reason: file_changed`) and moves it into `done/` on a later beat.

The body holds the title and the question body. `## Options` carries one `- [ ] N. <text>. Next: <next>. Pro: <pros>. Con: <cons>` line per option, with empty parts omitted. `## Context` carries the blocked-because, the options rationale, `Recommended: N, because <why>. Downside: <downside>`, the not-thought-through list, reversible with its cost, and meanwhile. The body ends with `## Answer`.

`questions.md` is the index: one `[[<file stem>|<title>]]` wikilink per open page under `## Open (N)`, newest ask first, and the 20 most recent closed pages under `## Done`. `questions.base` filters `file.hasProperty("question_id")` and carries four views: Needs you, Open by king, Open by node, and Board (cards grouped by status). Every view sorts newest ask first. Both files are generated. They carry a generated marker and are never written over a hand-authored file. The tick detail names it instead.

## The arm

The `attention` arm beats every 30 s, reads the projection, routes each item to its crown, writes pages, settles, records and closes. The settle state lives at `~/.fno/attention/questions.json`. The projection cache at `~/.fno/attention/items.json` gains `questions_dir`, which the king check-in reads. `attention.enabled = false` is the kill switch: it stops every page write and answer read, checked on every beat. The arm computes routing once at page-write time and never refreshes it: a crown crowned later never sees older pages in its check-in. When the graph is unreadable the beat delivers nothing (`skip_reason: routing_unreadable`) and the next beat retries.

## Config keys

The md config row is retired. There are two keys: `attention.enabled` (default true), the kill switch, and the `[[attention.sinks]]` rows above. A `[[attention]]` or `[[reach_me]]` row still in config.toml is ignored. The tick detail names the retired row once per beat.

## Answer lanes

No file answer records as `operator`. A file tick records `authority: file_edit` in the `attention_answer` row, and the `clear` that follows runs with no `--authority` flag. The durable row lands first, then the arm runs `clear`. If that clear times out, the row is already durable. The arm retries the clear and never writes a second row. First answer wins.

What a file answer can do: close the question, reach the asker, unblock the nodes in `blocks`. What it cannot do: become law, waive review coverage, supersede or retract a law row. A law-grade answer from a phone stays a coordination ruling until the user confirms it in chat through `/fno:law` or at a terminal.

## Failure modes

- The Mac sync client is closed: a phone tick never arrives, the page stays open, the delay is unbounded. Nothing is lost.
- Any local process can tick a box. The answer carries no identity the machine can check. The `attention_answer` row names the lane, and section-style limits hold.
- A page with conflict markers is skipped every beat and named in the tick detail. An answer typed inside it never records until someone resolves the markers.
- Another writer changes a page between the arm's read and its close write. That page is skipped (`skip_reason: file_changed`) and the next beat closes it. Other pages still process.

## The answer endpoint

`fno mux serve --attention-api` serves the contract's three paths on `127.0.0.1`, with its own router and port. `--attention-port` sets the port, default 8724. The endpoint runs beside the read-only web bridge, never inside it. `GET /v1/attention/items` lists the projection with `state`, `kind`, `ready` and `project` filters. `GET /v1/attention/items/{id}` returns one item. `POST /v1/attention/items/{id}/answer` records an answer. Remote reach is the user's own tunnel, `tailscale serve` on its own port. fno never binds a public address.

Every path requires a bearer token that matches a configured sink's `token_env` value. The token identifies the sink, never a person. A refused request lands no row. The statuses are `401` for a bad token, `404` for an unknown id, and `409` for a closed item. `422` covers a bad shape, an out-of-range option, `done` on a question, and more than one choice.

An accepted answer appends the same `attention_answer` row the arm writes. The authority stays `sink`. The idempotency key and any Tailscale Serve identity header ride the row as provenance. The endpoint then runs the same `fno inbox outstanding clear` the mux overlay runs. First answer wins. A retry under the same `idempotency_key` replays the receipt and lands no second row. A different key lands a superseded marker that changes nothing. When a clear fails, the row stays durable. The question then needs a terminal close, and the arm retries its own lane.

## The ntfy and webhook sinks

Sinks are `[[attention.sinks]]` rows in config.toml, read by the arm's beat beside the pages. An ntfy row carries `name`, `type = "ntfy"`, `url`, `topic`, `token_env`, and optionally `answer_base_url` and `body`. The `url` is the server root. The body mode is `"title-only"` by default, because anyone who knows a public topic reads it. `"full"` adds the recommendation and the downside. A webhook row carries the same shape with `type = "webhook"` and the adapter's URL.

When a sink's `answer_base_url` is a loopback address, the load refuses it. The tick detail names the refusal every beat. A phone cannot answer this machine's loopback. The supported setups are a tailnet address through Tailscale Serve, or a public URL behind the sink token.

The arm delivers each ready question or pin once per sink. The delivery id is `sha256(sink, item id, event)`. A transient failure retries next beat with the same id. The transient classes are connect errors, 5xx, 401, 403, 408 and 429, the status fanout's retry classes. A permanent 4xx drops the delivery for good. When a delivered item leaves the open set, the arm posts `item.closed`. A returned `external_id` rides that body. Outbound posts run one `curl` child per call. The URL, the bearer header and the body never appear in argv.

ntfy shows at most three actions per notification. A question therefore carries up to three `http` actions, with the recommended option first. Each action posts `{"option": N, "idempotency_key": "<delivery_id>:N"}` to the answer endpoint with the sink's bearer header. The one view link rides the `click` key. When the item has fewer options, a literal `view` button fills a free slot. A pin gets one `Done` action.

A webhook sink receives the `Delivery` body. The body carries `delivery_id`, `event` (`item.opened` or `item.closed`), `sink`, and the item. A sink with an `answer_base_url` also carries the `answer_url`. The adapter's `external_id` returns on `item.closed`. fno ships no adapter. The contract and the endpoint are the deliverable.

## Checks the user's agent runs

`fno-agents needs --items --json` prints what the pages will carry: the items, their readiness and the named sources. The user runs no command.

## Delivery

The arm's mux pass runs on every beat beside the pages. Every unsuperseded `attention_answer` row, whatever its sink, drives one reply ladder from the persisted state at `~/.fno/attention/replies.json`. First the clear: while the item is still open, the arm runs `fno inbox outstanding clear` (the same retry cap as the file lane, five). A note item skips the clear, because no door closes a note. Then the rungs, in order:

1. `mail` - the clear's mail leg. The posture line decides: `delivered (hosted)` is the end, `rung: mail, outcome: landed`.
2. `resume` - a durable park under an idle asker. When the asker has a session id, the arm runs `fno agents resume <session_id> --message <answer>`. This is the one resume door, with a 30 s bound and stdin closed. The message carries the question id and a dedupe line. A later beat looks for the question id in the asker's transcript, after the byte offset the resume saved. Seeing it confirms delivery (`rung: resume, outcome: confirmed`).
3. `crown` - an exit-17 resume, a nonzero exit, a missing transcript, or five unconfirmed minutes. The arm mails the answer, wrapped, to the live crown whose territory names the asker's node (`rung: crown, outcome: sent`). With no crown, the row reads `rung: none, outcome: failed` and the evidence names why.

Each item's ladder ends with one `attention_delivery` row in `questions.jsonl`. The panel shows the rung on the answered row for 15 minutes.

# Attention items

Questions and pins reach a user away from a terminal, and the answer returns. One model, no new store: an attention item is a read-time projection over the question journals, the escalation notes and the user lane. `fno-agents needs --items --json` prints it. The daemon's `attention` arm delivers it. The answer returns through `fno inbox outstanding clear`, the same door the mux overlay uses.

Question pages need no setup. The attention arm writes one page per open question or pin into the vault's questions folder whenever it beats. Resolve it with `fno-agents state path questions`: with obsidian enabled it is the project's questions folder under the vault's internal tree, without it `<space>/questions`. One page per question, a `done/` folder for closed pages, a `questions.md` index, and a `questions.base` Obsidian Base over the folder. `[[attention]]` and `[[reach_me]]` md rows are retired: a row still in config.toml is ignored and named once per beat in the tick detail.

## Is this page for you?

The stakes are the user's attention and the fleet's unblocking: a delivered question the user cannot answer holds work. Not for one-way event pushes to phones and webhooks; that is the status fanout in [status-fanout](../status-fanout.md). For the mux's own needs overlay, use the mux.

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

The context fields: `asker`, `node`, `blocked_because`, `options_rationale`, `recommendation` (the pick, its reason, its downside), `options` (each with next, pros and cons), `unknowns`, `reversible` (`yes`, `costly`, `no`), `cost_if_wrong`, `meanwhile`. A question missing a required field is `ready: false`, and `fno-agents needs --items --json` prints the `missing` list so the asker can re-ask with the fields.

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
outstanding: refused: a question needs what, why, two options, a recommendation. Write a question file (docs/architecture/attention-items.md, "Asking with context") and pass --question-file. One action with no choice is a pin: pass --ask "<the action>".
```

A reversible question that carries a recommendation is one the asker must decide itself. The port refuses it too, naming the door: record the ruling as the asking session or its king with `fno backlog decide <node> "<ruling>"`, then continue. It reaches the user only with a user-only reason in a `why_user:` frontmatter key: irreversible, spends money or a credential, reaches outside the machine, or a product or taste call. The user sees a decide-it-yourself ruling only as a one-line FYI in the check-in, never as a question.

## Question pages

One page per question. The page name is `<ask date>-<question id>-<slug>-<node>.md`, for example `20260922-q-aaaaaaaa-wont-do-deferral-kind-x-bbbb.md`. A file is a page only when its frontmatter `question_id` is the id inside its file name, so a sync client's `q-1 (conflicted copy)` is ignored. Closed pages move to `done/<id>.md` (`<id>-2.md`, `-3.md`, ... when the name is taken).

Frontmatter carries `question_id`, `kind`, `status`, `title`, `aliases`, `asked_at`, `project`, `harness_session_id`, `session_name`, `harness`, `model`, `node`, `blocks`, `epic`, `crown`, `king`, and on close `answer`, `answered_at`, `recorded_by`. An unmeasured asker fact reads `unknown` and an absent routing fact reads `none`, never a blank key. The renderer escapes `<` and `>` in every text field it writes, so an unescaped `<stage>` never eats the checkboxes after it; the answer reader unescapes them.

The body holds the title, the question body, `## Options` with one `- [ ] N. <text>. Next: <next>. Pro: <pros>. Con: <cons>` line per option with empty parts omitted, `## Context` lines for the blocked-because, options rationale, `Recommended: N, because <why>. Downside: <downside>`, not-thought-through, reversible with its cost, and meanwhile. It ends with `## Answer`. An unticked `- [ ]` line is never an answer. One ticked option records `option: N`; two ticked options record nothing and earn one notice; words under `## Answer` record as the answer; a pin's `- [x] Done` records `done`. After the settle window (120 s), a changed page restarts the window; a frontmatter stamp does not (the settle key is a body hash). First answer wins. A close re-reads the page and skips it (`skip_reason: file_changed`) when another writer changed it, then moves it into `done/`.

`questions.md` is the index: one `[[<file stem>|<title>]]` wikilink per open page under `## Open (N)`, and the 20 most recent closed pages under `## Done`. `questions.base` is an Obsidian Base filtering `file.hasProperty("question_id")` with views Open by king, Open by node, and Board (cards grouped by status). Both are generated: they carry a generated marker and are never written over a hand-authored file; the tick detail names it instead.

## The arm

The `attention` arm beats every 30 s, reads the projection, routes each item to its crown, writes pages, settles, records and closes. The settle state lives at `~/.fno/attention/questions.json`; the projection cache at `~/.fno/attention/items.json` gains `questions_dir`, which the king check-in reads. `attention.enabled = false` is the kill switch: it stops every page write and answer read, checked on every beat. The arm computes routing once at page-write time and never refreshes it: a crown crowned later never sees older pages in its check-in. When the graph is unreadable the beat delivers nothing (`skip_reason: routing_unreadable`) and the next beat retries.

## Config keys

The md config row is retired. There is one key: `attention.enabled` (default true), the kill switch. A `[[attention]]` or `[[reach_me]]` row still in config.toml is ignored; the tick detail names the retired row once per beat.

## Answer lanes

No file answer records as `operator`. A file tick records `authority: file_edit` in the `attention_answer` row, and the `clear` that follows runs with no `--authority` flag. The durable row lands first, then the arm runs `clear`. If that clear times out, the row is already durable; the arm retries the clear and never writes a second row. First answer wins.

What a file answer can do: close the question, reach the asker, unblock the nodes in `blocks`. What it cannot do: become law, waive review coverage, supersede or retract a law row. A law-grade answer from a phone stays a coordination ruling until the user confirms it in chat through `/fno:law` or at a terminal.

## Failure modes

- The Mac sync client is closed: a phone tick never arrives, the page stays open, the delay is unbounded. Nothing is lost.
- Any local process can tick a box. The answer carries no identity the machine can check. The `attention_answer` row names the lane, and section-style limits hold.
- A page with conflict markers is skipped every beat and named in the tick detail; an answer typed inside it never records until someone resolves the markers.
- Another writer changes a page between the arm's read and its close write. That page is skipped (`skip_reason: file_changed`) and the next beat closes it. Other pages still process.

## The other two sink types, named and not built

`ntfy` brings action buttons and needs an answer endpoint the phone can reach. `webhook` is the escape-hatch contract. Both stay specified in the plan and unbuilt until the pages have been observed. When they ship, this page grows their sections; ntfy will link a page path instead of a block anchor.

## Checks the user's agent runs

`fno-agents needs --items --json` prints what the pages will carry: the items, their readiness and the named sources. The user runs no command.

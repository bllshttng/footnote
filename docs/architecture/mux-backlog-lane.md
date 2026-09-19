# The mux backlog lane

The `~ backlog` section of the mux sideline: the board cards, their scope, and the node detail overlay that turns a card into a work surface. This lane was undocumented until the node detail landed. This page is its contract.

## The cards

One card per non-done node, derived read-only from `graph.json` through the store keeper (`backlog_view.rs`). The card is a `BacklogCard`: id, slug, priority, state (`Ready`, `Blocked`, `InFlight`), and the route fields (`pane_id`, `attach_id`, `where_hint`).

The card label leads with the id, then the slug: `<id> <slug>`. The id is the handle every verb takes, so it renders first and the slug follows it. Every paint site (the card row, the mini-kanban, the navigator, the card menu header) folds through one `card_label` helper. The four rows cannot drift apart.

A claim flips a card to in-flight without a graph write. The sideline folds the live claim store every tick (`fno-agents claim sweep`). The claim lockfile outranks the graph's status field.

A PR row names the session driving it. An agents-section row carrying a PR shows `attach <short id>` beside its name, joined server-side from the graph: the live claim holder's session, else the node's last do or ship session from `sessions[]`. A PR row whose node records no session says `no session`.

## The scope

The board is scoped at client spawn. The spawner resolves `mux.board_scope` (values `repo`, `all`, `workspace:<name>`), latches the answer into `FNO_BOARD_SCOPE`, and the server reads only that env. The reason rides under the `~ backlog` header as the section's subline, in the latch's own words:

```
~ backlog
scope: latched at spawn: project fno
```

An empty or surprising board answers WHY it looks the way it does. A hand-started server (no latch) says so and shows every project rather than narrowing silently. `fno mux doctor` resolves live and reports what a fresh spawn will latch.

## The node detail overlay

Enter on a card opens the node's detail. The record folds off the UI loop: `fno backlog get <id>` (its JSON record), a ten-second budget, `kill_on_drop` on timeout. A fold failure renders its typed reason on one line (timeout names its budget, a refusal carries the store's stderr). A slow read never paints a fabricated empty pane.

The pane shows, in order:

| Section | Content |
|---|---|
| Header | the node id, then its title (slug as fallback) |
| Meta | status, project, priority, and `king: <name> (L<level>)` or `king: none` |
| Plan | the node's `plan_path`, or `none` |
| Sessions | one row per `sessions[]` entry |
| Comments | `notes (N) · decisions (N)` plus the newest three notes and decision ids |

### The session row

Columns: phase, harness, short id (first 8 chars, the join key), model (observed at event time, registry model as fallback), state, age, action, basis.

The state and age come from the joined registry row (`AgentRow`), joined by `harness_session_id`. An unmeasured row says `unmeasured`. An absent age renders `-`. The pane never shows a state it did not measure.

The basis column names where the row came from. `claim` says the store records that session as the node's live claim holder. `graph` says the row exists because `sessions[]` recorded it.

### The launch action

The action cell is derived, never a fixed verb:

| Condition | Action |
|---|---|
| the joined row has a pane or an attach id | `attach` (focus or attach) |
| the row is resumable and its registry state is outside working and done | `resume` |
| no registry row | dim: `no registry row` |
| registry state done or working, no route | dim: `done` / `working` |
| a registry row with no route and not resumable | dim: `not resumable` |

Enter, `a`, or `r` on the selected row runs the agents section's own hit cascade (`agent_hit`). The cascade focuses the pane, else attaches, else resumes. A refusal is the notice. A dim row answers with its reason and launches nothing. Esc closes the overlay and leaves the selector open underneath.

### The king

A node's king is a reverse lookup, not a field. The crown lives on the registry row (`crown_level`, `crown_scope`). The overlay reads every crowned row and splits its scope on `,`. It names the first row whose territory contains the node id, its parent epic, or its project. Direct membership only: a grandchild epic resolves through no scope, and the pane says `none` rather than guessing.

## The Plan menu entry

The card menu (right-click or `m`) carries a `Plan` entry. It runs the same dispatch door as a card click, pinned to the architect sub-agent with the blueprint message (`/fno:blueprint <id>`). `Command::DispatchPlan` rides the wire, the server re-checks readiness, then the door's own spawn gate and placement lease answer. A refusal renders verbatim. Nothing is synthesized.

## Keys

| Key | Where | Does |
|---|---|---|
| Enter | on a selected card | opens the node detail |
| j / k, arrows | in the detail | move the session selection |
| Enter, a, r | on a selected session row | run its derived action |
| Esc | in the detail | close (the selector survives underneath) |
| m, right-click | on a card | the card menu: float, defer, plan, open plan |

## Out of scope here

Rulings that gate a node (a decision naming the nodes it controls) need a write door that stamps `DecisionRef` rows. The feed carries the projection, and the schema does not yet name its nodes. The public web board shows every project grouped by domain. Reworking it to the detail shape is its own node. A Seed action for a Done row waits on resume's seed mode. The cell renders the dim `done` until then.

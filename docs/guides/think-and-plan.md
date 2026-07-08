# Think and Plan

Design before you build. These two skills turn vague ideas into concrete, testable implementation plans.

## Think

```
/fno:think "notification system for our app"
```

Think is a structured brainstorming session. It asks you questions one at a time - who are the users, what are the constraints, what does success look like - and generates BDD acceptance criteria (Given/When/Then) alongside the design.

### What happens

1. **Context gathering** - reads your project, checks existing specs
2. **Scope check** - if the idea is too big, helps you decompose it into sub-projects
3. **Interactive exploration** - asks questions (prefers multiple choice) to refine the idea
4. **Multi-perspective analysis** - considers the problem from different angles
5. **Failure mode enumeration** - produces a required `## Failure Modes` section covering boundaries, errors, invariants, and concurrency hazards
6. **Design output** - presents a design for your approval

Think will not let you skip to implementation. It has a hard gate: no code until you approve the design.

### Failure Modes (required section)

Every design doc saved by `/think` includes a level-2 `## Failure Modes`
heading with four sub-sections:

| Sub-section | What goes here |
|-------------|----------------|
| **Boundaries** | Limits, edge values, empty states, maximums, overflow |
| **Errors** | Failure paths from dependencies (API 500, DB deadlock, timeouts) |
| **Invariants** | Rules that must hold (referential integrity, monotonic counters) |
| **Concurrency** | Ordering, race conditions, double-submits, out-of-order events |

Each bullet is one sentence in imperative form: "The system must handle...",
"must reject...", or "must preserve...". This turns failure-mode worries
into testable obligations that `/blueprint` can seed directly as AC4-EDGE
acceptance criteria in the implementation plan.

For trivial features (pure functions, no I/O, no state) the heading is
still required but the content can be a one-line justification for "none".
Skipping the heading is not allowed - `/blueprint` refuses to plan without it.

If `/think` detects a complex feature (3+ external dependencies, auth,
payments, concurrency) it will recommend running `/think what-if` for a deeper
failure-mode pass and emit a copy-paste hand-off line for you.

### When to use think

- Starting a new feature from scratch
- Exploring a vague idea before committing
- You want acceptance criteria before writing tests
- You need to decompose a large project into pieces

### Think outputs

Think doesn't create files by default. It presents the design in conversation. If you want to save it, say "save this" or move on to `/fno:blueprint` which captures everything in files.

## Plan

```
/fno:blueprint "notification system"
```

Plan creates an implementation strategy with waves, tasks, and file ownership. It produces a plan directory with an index file and phase files.

### What it creates

```
plans/notification-system/
  00-INDEX.md         Execution strategy, wave definitions, metadata
  01-core-api.md      Phase 1 tasks, files, acceptance criteria
  02-ui-components.md Phase 2 tasks
  03-integration.md   Phase 3 tasks
```

### Plan modes

| Mode | Command | Output |
|------|---------|--------|
| **Full** (default) | `/fno:blueprint "feature"` | Multi-phase folder with waves, BDD criteria |
| **Quick** | `/fno:blueprint quick "feature"` | Single plan file, lightweight |

Quick plans work with `/fno:do`. Full plans work with `/fno:do waves` or `/fno:target`.

### Refusal when a design doc is missing Failure Modes

When you pass `/blueprint` a path to a design doc, it runs a grep gate before
planning: if the doc does not contain a `## Failure Modes` heading, `/blueprint`
halts with:

```
Design doc at {path} is missing ## Failure Modes section. Run /think first.
```

This is deliberate. The purpose of the gate is to keep failure-mode
thinking inside `/think` where you can still iterate, not to patch over a
skipped step in `/blueprint`. If this fires, run `/think` on the feature to
regenerate the design doc with the required section.

When a design doc passes the gate, `/blueprint` parses the four sub-sections
(Boundaries / Errors / Invariants / Concurrency) and seeds AC4-EDGE
acceptance criteria in each phase file, citing the source bullet by name.
Full mode emits one citation per relevant bullet per phase; quick mode
inlines the citations under the relevant Change in the Changes section.

If you pass `/blueprint` a raw feature description instead of a file path, the
gate is skipped - there is no file to scan.

### Wave-based execution

The 00-INDEX.md defines which tasks run sequentially and which run in parallel:

```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    tasks: [1.1]
  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2, 2.3]
  - wave: 3
    mode: sequential
    tasks: [3.1]
```

Wave 1 completes before wave 2 starts. Tasks within wave 2 run in parallel (via subagents). Wave 3 waits for all of wave 2.

### Claiming an existing idea node

If `/think` (or any earlier session) has already filed a placeholder
on the backlog graph for the work you're about to spec, pass that node's
`ab-XXXXXXXX` id directly to `/blueprint` instead of a free-text description:

```
/fno:blueprint ab-XXXXXXXX
```

`/blueprint` resolves the title and details from the graph, writes
`claims: ab-XXXXXXXX` into the rendered plan frontmatter, and refuses
to adopt unless that line is present. At intake, the claim updates the
existing idea node in place rather than creating a duplicate. This
prevents the kanban from accumulating dangling placeholders every time
a /think session lands a follow-on spec.

To repair a past mistake (a plan was adopted as a fresh node when it
should have claimed an existing idea), run:

```
fno backlog intake path/to/plan --claims ab-XXXXXXXX
```

See [docs/architecture/plan-claims.md](../architecture/plan-claims.md) for
the full contract, refusal paths, and mutator semantics.

### Plan then execute

The typical flow:

```
/fno:think "notification system"     # explore and approve design
/fno:blueprint "notification system"      # create the plan
/fno:target path/to/plan/             # execute it
```

Or skip think if you already know what you want:

```
/fno:blueprint "add retry logic to API calls"
/fno:do path/to/plan.md
```

## Panel: Multi-Persona Debate

While `/think` is a collaborative 1-on-1 session, `/think panel` assembles
a panel of opinionated personas to debate your product decisions. You get
a formal seat on the panel as "Domain Expert" by default.

```
/think panel "should we add a free tier"
/think panel deep "rebuild auth or iterate"
/think panel startup "is this the right market"
/think panel auto "quarterly roadmap priorities"
```

### Modes

| Mode | Command | Description |
|------|---------|-------------|
| Interactive (default) | `/think panel "question"` | You're on the panel with structured prompts at each phase |
| Autonomous | `/think panel auto "question"` | Panel runs without pauses, briefing phase gathers ground truth upfront |
| Continue | `/think panel continue {slug} "new data"` | Resume a prior debate with new information |

### When to use think vs panel

| Scenario | Use |
|----------|-----|
| Designing a specific feature | `/think` - you need collaborative design, not debate |
| Deciding WHAT to build next | `/think panel` - you need multiple perspectives |
| Exploring technical approach | `/think` - deep 1-on-1 exploration |
| Evaluating a pivot | `/think panel` - stress-test from all angles |
| Quick brainstorm | `/think` - fast, lightweight |
| Strategic decision with tradeoffs | `/think panel` - structured debate with consensus |

### Depth presets

| Depth | Built-in Personas | Rounds | Base Time | Base Tokens |
|-------|-------------------|--------|-----------|-------------|
| shallow | 2 + DA | 1 | ~3 min | ~20K |
| standard (default) | 4 + DA | 2 | ~7 min | ~50K |
| deep | all + DA | 3 | ~12 min | ~80K |

Project default personas (from config.toml) and user seat are always included on top.

### Subcommands and flags

Subcommands go before the decision text. Flags use dashes.

```
# Subcommands (positional)
/think panel deep "question"               # depth
/think panel shallow "question"            # depth
/think panel startup "question"            # persona set
/think panel adversarial "question"        # persona set
/think panel auto "question"               # autonomous mode
/think panel continue {slug} "new info"    # resume session

# Flags (combine with any mode)
/think panel --chain plan "question"       # chain to another skill after
/think panel --rounds 3 "question"         # override debate rounds
/think panel --no-user-seat "question"     # opt out of panel seat
/think panel --user-role "Inspector" "q"   # custom seat name
```

### Session continuation

Every session saves structured state. Resume later with new information:

```
/think panel "pricing strategy"
# ... panel produces recommendations ...
# ... days later, you have new data ...
/think panel continue pricing-strategy "engagement data shows opens spike around inspection time"
# Panel revisits recommendations with new context

/think panel continue list                 # see all saved sessions
```

### Chaining

The panel produces a report with ranked recommendations. Chain directly
into design or planning:

```
/think panel --chain think "add notifications"    # debate then design
/think panel --chain plan "add notifications"     # debate then plan
```

### Persona sets

Three built-in sets, plus project defaults from config.toml, plus custom inline:

- **default** - PM, Designer, Developer, Target User, CEO, Devil's Advocate
- **startup** - Solo Founder, Target User, Churned User, Competitor, Investor, Devil's Advocate
- **adversarial** - Skeptical Customer, Regulatory Reviewer, Accessibility Advocate, Scale Pessimist, Budget Hawk, Devil's Advocate

Configure project-specific personas in `.fno/config.toml`:

```yaml
think_tank:
  default_personas:
    - name: "Regulatory Expert"
      role: "Licensing analyst"
      expertise: "Compliance, citation patterns"
      bias: "Risk-averse, process-oriented"
```

These merge with the built-in set automatically.

## Database-Aware Planning

When your project has a database (Supabase, Postgres, Prisma, Drizzle), spec automatically detects it and appends schema context to the codemap. This means plans include migration tasks when needed, rather than discovering constraint violations at runtime.

```
/fno:codemap --db              # Manually include DB schema
```

Spec auto-detects databases and runs this for you. The DB schema section shows enums, CHECK constraints, triggers, and foreign keys so the plan knows what the database accepts before writing insert code.

You can also run codemap with `--db` directly to inspect your schema at any time.


## What-If

```
/fno:think what-if "what if we used WebSockets instead of polling?"
```

What-if is a scenario exploration tool. It takes a question and explores multiple angles: what could go wrong, what are the tradeoffs, what edge cases exist. Useful before committing to an approach.

Unlike think, what-if doesn't produce acceptance criteria. It produces analysis.

### Deep-dive mode for /think

`/think what-if` also serves as the deep-dive companion to `/think`. When a
feature is complex enough that inline failure-mode enumeration isn't
enough (many external deps, auth, payments, concurrent state), `/think`
emits a hand-off line like:

```
Run `/think what-if software deep failure-modes "checkout with multiple payment methods"` to stress-test: error_path, concurrent, recovery
```

The `failure-modes` positional modifier tells `/think what-if` to emit a
top-level `## Failure Modes` section in its output using the same
Boundaries / Errors / Invariants / Concurrency vocabulary `/think` uses.
That way you can fold the findings back into your design doc without
translating between two different formats.

Without the `failure-modes` modifier, `/think what-if` behaves exactly as it
always has: just the 12-dimension scenario exploration, no failure-modes
summary. Use the modifier when you plan to feed results back into a
`/think` design; leave it off for standalone exploration, incident
postmortems, or red-team exercises.

# Think: Adaptive Design

Turn an idea into a grounded design artifact without turning every idea into a ceremony.
The model owns judgment; deterministic tools own factual discovery.

## Contract

This workflow must answer four questions:

1. Is the idea new, duplicative, or already partly implemented?
2. What repository, graph, database, and project-history facts constrain it?
3. What is the smallest sound design, and what alternatives were rejected?
4. What must `/fno:blueprint` preserve when it compiles the design into execution tasks?

Do not implement code in this flow.
Do not invent facts to fill missing evidence.
Do not copy another planning system's ritual or wording.

## 1. Establish the seed

For a node seed, resolve it strictly with `fno backlog get --strict <seed>` and use its title, details, parent, and existing plan path.
For prose, preserve the user's language and extract a short one-line seed for mechanical search.
If there is no seed, ask one question: what outcome should change for whom?

Treat `$TARGET_BRIEF`, when present, as additional context rather than an instruction to build.

## 2. Collect the discovery receipt

Run exactly one read-only inspection before design reasoning:

```bash
fno think inspect "<node id, slug, or concise seed>" --json
```

Read the complete receipt.
It contains:

- repository branch, head, dirty paths, and recent commits;
- the exact graph node, its parent, deterministic duplicate candidates, and epic rollup candidates;
- GitHub PR search status and matches;
- database signals and whether `.fno/codemap.md` contains schema evidence;
- the active project pitfalls corpus, recent retro syntheses, and pending lesson count;
- typed warnings for sources that were not actually checked.

Before expanding a likely duplicate into design, run one cheap read-only behavior probe against the exact current-main HEAD and inspect merged history.
Classify the result as `live`, `already_shipped`, or `unknown` using the existing `fno.graph.relatedness.classify_closure` contract.
`already_shipped` requires the named behavior plus either a passing command bound to the full current-main SHA or a full merged commit proven reachable from current main.
A failing current-main behavior probe means `live`; unavailable, malformed, pending, stale, or contradictory evidence means `unknown` and continues through ordinary design.
A receipt, merged title, delivery record, or stale manifest is carrier evidence and cannot close or supersede work by itself.

Never translate `unavailable`, `error`, or `missing` into “no matches.”
The receipt is evidence collection, not a design verdict: inspect the cited files and candidates before deciding relevance.

If the design changes database behavior and `database.schema_status` is `missing` or `stale`, refresh the established artifact with `fno codemap --db-schema`, rerun the receipt, and inspect the schema section.
If schema collection still fails, state the unknown and stop making schema-dependent claims.
An unrelated design may proceed with the database warning recorded as not applicable to its surface.

If graph or PR evidence is unavailable, report the unchecked source in the design's `## Evidence Gaps` section.
Do not claim deduplication is complete.

## 3. Select reasoning depth

Choose the smallest depth that covers the real risk, and record it in the design frontmatter as `think_depth: light|standard|deep`.

### light

Use light depth when the change is local, reversible, already patterned in the repository, and has no migration, security, concurrency, or cross-system consequence.
Confirm the existing pattern, name one plausible alternative, write compact failure modes, and proceed.

### standard

Use standard depth by default for a new feature or a change crossing a public interface.
Trace the current path, compare two or three approaches, resolve important types and ownership, and write behavior-level acceptance criteria.

### deep

Use deep depth when evidence is contradictory or the change touches data migration, authentication, money, destructive operations, concurrency, multiple repositories, a new runtime, or a hard-to-reverse public contract.
Trace every reachable path, test the design against project pitfalls, and use `/fno:think what-if failure-modes "<scope>"` as a separate handoff when the risk surface exceeds a compact inline analysis.

Depth follows risk and uncertainty, not the user's requested feature size alone.

## 4. Resolve material uncertainty

Read the smallest set of repository sources that can answer the open questions:

- project instructions and architecture docs;
- implementation and tests on the current path;
- relevant recent commits from the receipt;
- exact graph and PR candidates;
- schema evidence when data is involved;
- applicable pitfall and retro sources.

Search for analogous behavior and call sites before proposing a new abstraction.
Distinguish “not found” from “not searched.”

Ask one material question at a time only when repository evidence cannot answer it and different answers would materially change the design.
Prefer concrete choices with their tradeoffs.
Do not ask the user to choose facts that the codebase can reveal.

## 5. Frame the problem

Write a short problem statement covering:

- the user or operator outcome;
- the current behavior and evidence;
- the constraint that makes the change non-trivial;
- explicit non-goals;
- how success will be observed after shipping.

If the receipt found overlapping work, decide explicitly whether to reuse, extend, relate, supersede, or stop as a duplicate.
Use `fno backlog update` for any graph relationship; never hand-edit the graph.

## 6. Compare approaches

Offer two or three genuinely different approaches when a real architectural choice exists.
For each, state the core shape, advantages, costs, and why it fits or conflicts with repository patterns.
Lead with a recommendation.

For a light design with an obvious established pattern, one recommended approach plus one rejected alternative is enough.
Do not manufacture alternatives to satisfy a count.

Prefer improving an existing primitive over creating a parallel source of truth, graph, state machine, or orchestration layer.
Name the owner of each new durable state and the command or code path allowed to mutate it.

## 7. Resolve the design

Specify only the interfaces needed to remove implementation ambiguity:

- inputs and outputs;
- state owner and lifecycle;
- error vocabulary and exit behavior;
- compatibility boundary;
- concurrency or idempotency rule when relevant;
- observability proving the outcome.

Use discriminated result types when downstream behavior differs by outcome.
Avoid boolean flags whose combinations create invalid states.
Do not freeze private implementation details that the executor can decide safely.

### User-facing surfaces

Only when the change has a real UI or interaction surface, cover the applicable states: empty, loading, success, partial, error, retry, cancellation, and repeated action.
Describe the transition and recovery behavior, not just the happy screenshot.
Use a diagram or visual companion only when spatial or state relationships are materially clearer than prose.

For CLI, hook, or control-plane work, treat stdout, stderr, exit codes, artifacts, locks, and retries as the interaction states.

## 8. Write failure modes

Every design must contain this top-level section because `/fno:blueprint` consumes it:

```markdown
## Failure Modes

**Boundaries**
- Applicable limits, empty inputs, or “not this surface” cases.

**Errors**
- Dependency failures, corrupt evidence, and user-visible recovery.

**Invariants**
- Facts that must remain true across every reachable path.

**Concurrency**
- Races, duplicate actions, lock behavior, or “not concurrent” with evidence.
```

Keep all four bold labels even in a light design.
Write one concise applicable statement under each; “not applicable because …” is better than invented risk.
For deep designs, trace guards across every reachable path and name the production choke point where the invariant is enforced.

## 9. Write acceptance criteria

Use behavior-level Given/When/Then scenarios for the important outcomes.
Read `skills/tdd/references/bdd-acceptance-criteria.md` and its directly relevant supporting reference when the surface needs fuller patterns.

Cover the happy path plus only the edge, error, abuse, accessibility, or concurrency cases justified by the design.
Every criterion must be observable and later verifiable through a real command, test, artifact, or external state.
Do not require a specific private implementation unless it is itself the contract.

Explicit AC identifiers such as `AC1-HP`, `AC2-ERR`, and `AC3-CON` are optional author input: supply them when you want a stable hand-written cite, and Blueprint preserves them verbatim.
When you omit them, write the criterion as a numbered item, a bullet, a heading, a table row, or a descriptive label followed by a Given/When/Then block; Blueprint compiles the section and assigns deterministic `AC1`, `AC2`, ... identifiers in document order, so every accepted shape is equally citable.
What is required is observable, verifiable behavior, not one Markdown spelling; formatting is normalized, not enforced.

## 10. Save the design

Use the canonical path resolver; never hand-assemble a date or node suffix:

```bash
fno plan path --slug "<feature-slug>" [--node "<node-id>"]
```

For a node seed, reuse an existing design that already claims the node or whose filename ends in the canonical node id.
For prose without a node, omit `--node`; Blueprint can intake and rename it later.

Use semantic line breaks: one complete prose sentence per physical line.
The design should contain, as applicable:

```markdown
---
title: <title>
status: design
type: think
think_depth: light|standard|deep
claims: <node id, when seeded>
sources: [<artifacts actually read>]
---

# <Title>

## Problem
## Evidence
## Evidence Gaps
## Non-Goals
## Recommended Design
## Alternatives Considered
## User Experience
## Domain Pitfalls
## Failure Modes
## Acceptance Criteria
## Open Questions
```

Omit empty optional prose sections, but never omit `## Failure Modes`.
List only sources actually inspected; a receipt path is not proof that every referenced artifact was read.

For a node-seeded design, link the durable artifact through the CLI:

```bash
fno backlog update "<node-id>" --plan-path "<doc-path>"
```

When a target manifest exists with an empty `plan_path`, first-fill it only through `fno state set --type target --field plan_path --value "<doc-path>"`.
Never edit the manifest directly.

## 11. Review proportionally

Before handoff, check:

- every recommendation is supported by inspected evidence or labeled inference;
- duplicate candidates and evidence gaps have explicit dispositions;
- the design improves an existing primitive where possible;
- failure modes cover the actual production paths;
- acceptance criteria verify outcomes rather than ceremony;
- scope and non-goals are crisp enough for Blueprint to decompose.

A light design can self-review once.
A standard design should perform one adversarial pass.
A deep design should use an independent reviewer or `/fno:think what-if` when available, but reviewer unavailability is recorded rather than faked.

## 12. Handoff

Report the design path, node relationship, chosen depth, duplicate disposition, database grounding status, unresolved evidence gaps, and recommended next action.

Use one of these boundaries:

- `/fno:blueprint <design-path>` to compile the design into Footnote execution tasks;
- native Claude or Codex Plan Mode when the intent is already sufficiently resolved and the next bare `/fno:target` will capture the approved plan;
- `/fno:think what-if failure-modes "<scope>"` when a deep risk surface needs separate stress testing;
- stop when the receipt reveals the work is already done or truly duplicative.

Think is optional depth, not a tax on every target.
The deterministic receipt is reusable whether intent began in conversation, native Plan Mode, or a Footnote design document.

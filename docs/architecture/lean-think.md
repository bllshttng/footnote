# Lean Think and Native Planning

Footnote separates factual discovery, research, and execution compilation.
This keeps strong project grounding without forcing every request through the same amount of ritual.

## The three layers

`fno think inspect` is the deterministic discovery layer.
It reads repository, backlog, PR, database-artifact, and project-lesson evidence and emits typed source status without mutating code or state.

`/fno:think` is the research layer.
It investigates a question against primary sources (source code, specs, first-party APIs) and writes cited findings to one Markdown file. The question varies by brief - what is true (think), how it breaks (what-if), several lenses (panel), what owns a concern (class) - but the three-step process (ground, investigate, write) is fixed.

`/fno:blueprint` is the execution compiler.
It turns an approved design into owned tasks, dependencies, verification commands, and graph relationships.

Native Plan Mode can replace conversational intent capture or produce an already approved implementation plan.
It does not replace deterministic project discovery when the plan makes claims about existing code, backlog duplication, pull requests, schema, or project pitfalls.
The next bare `/fno:target` may capture an approved native plan, while Blueprint remains appropriate when Footnote graph decomposition and execution contracts are needed.

## Why this is Footnote-specific

The workflow does not copy Superpowers.
It independently keeps the useful separation between exploring a design and compiling an implementation plan, while grounding both in Footnote's graph, schema artifact, worktree ownership, failure-mode handoff, and durable node lifecycle.

Visual aids are optional and evidence-driven.
Footnote should add one when a state machine, graph, or spatial interface becomes easier to review visually, not operate a companion server as mandatory ceremony.

## Truth boundary

The receipt reports source status, not a conclusion.
An unavailable GitHub search is not “no matching PR,” missing or stale schema evidence is not “no database change,” and a graph read failure is not “no duplicate.”
The model decides relevance only after inspecting the cited evidence.

The receipt is derived and read-only.
Backlog changes still go through `fno backlog`, target manifest first-fill still goes through `fno state set`, and schema refresh still goes through the established `fno codemap --db-schema` artifact path.

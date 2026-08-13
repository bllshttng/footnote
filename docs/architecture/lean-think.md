# Lean Think and Native Planning

Footnote separates factual discovery, research, and execution compilation.
This keeps strong project grounding without forcing every request through the same amount of ritual.

## The three layers

`fno think inspect` is the deterministic discovery layer.
It reads repository, backlog, PR, database-artifact, and project-lesson evidence and emits typed source status without mutating code or state.

`/fno:think` is the research layer.
It investigates a question against primary sources (source code, specs, first-party APIs) and writes cited findings to one Markdown file. The brief sets the question. The three-step process is fixed: ground, investigate, write.

`/fno:blueprint` is the execution compiler.
It turns an approved design into owned tasks, dependencies, verification commands, and graph relationships.

The discovery receipt is shared infrastructure.
`/think` uses it as the first step of research.
When no cited findings are supplied, `/blueprint` uses the receipt to ground itself. A fresh plan never rests on questions alone.

Native Plan Mode can replace conversational intent capture or produce an already approved implementation plan.
It does not replace deterministic project discovery when the plan makes claims about existing code, backlog duplication, pull requests, schema, or project pitfalls.
The next bare `/fno:target` may capture an approved native plan, while Blueprint remains appropriate when Footnote graph decomposition and execution contracts are needed.

## Why this is Footnote-specific

The workflow does not copy Superpowers.
It independently keeps the useful separation between exploring a design and compiling an implementation plan.
Both layers ground in Footnote's graph, schema artifact, worktree ownership, discovery receipt, and durable node lifecycle.

Visual aids are optional and evidence-driven.
Footnote should add one when a state machine, graph, or spatial interface becomes easier to review visually, not operate a companion server as mandatory ceremony.

## Why the reasoning lives in the model

The old `/think` skill carried 98 KB of design-reasoning guidance.
It is gone from the text on purpose, not by oversight.
None of that text was run while designing this change, and the design held.
The reasoning already lives in the model, so prose that reads well but changes no plan stays deleted.

One part earned a place in `/blueprint`: the deterministic discovery receipt.
It changes the plan a deterministic step can produce. A plan grounded on duplicate candidates, schema status, and the active pitfalls differs from one guessed from questions alone.
Everything else stayed deleted. The depth ladder and the four-label failure vocabulary carried a scale-to-risk escape hatch or changed nothing.

## Truth boundary

The receipt reports source status, not a conclusion.
An unavailable GitHub search is not “no matching PR,” missing or stale schema evidence is not “no database change,” and a graph read failure is not “no duplicate.”
The model decides relevance only after inspecting the cited evidence.

The receipt is derived and read-only.
Backlog changes still go through `fno backlog`, target manifest first-fill still goes through `fno state set`, and schema refresh still goes through the established `fno codemap --db-schema` artifact path.

---
name: ship
description: "Drive any deliverable to its finish line. The umbrella over delivery terminals: 'ship pr' is the PR lifecycle (= today's /pr), 'ship doc' ships a research brief to output_dir and grades it. Use when: 'ship this', 'ship a PR', 'ship the doc', 'ship the brief', 'deliver this'. Not for ongoing areas (budget, community) - those have no finish line; use /target or /megawalk."
argument-hint: "<pr|doc>  (pr: create|check|merged - the PR lifecycle; doc: <topic> [--golden <discovery-*.md>])  - a type is required, there is no default"
requires:
  binaries:
    - "fno >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.0"
---

# Ship

**One verb for delivering anything.** `/ship <type>` drives a deliverable to its finish line, dispatching on the *deliverable type* the way `/target` dispatches on task type. `/pr` only names the code branch; `/ship` names the whole family.

| Type | Finish line (the mechanical "green") | What runs |
|------|--------------------------------------|-----------|
| `pr` | PR exists + CI green + required bot reviewed, no unaddressed blocking finding (`DonePRGreen`) | the `/pr` router (`create` / `check` / `merged`) |
| `doc` (alias `artifact`) | brief written to `config.research.output_dir` + `fno evals grade` green (`DoneAdvisory`) | [doc.md](references/doc.md), in this same context |

## The membership test (load-bearing)

A thing is a ship type ONLY if it has a definable **green** - a finish line readable mechanically. `pr` and `doc` both do. An ongoing *area* (`budget`, `community`) has no crisp green: it never "finishes", so it is not a deliverable. Admitting areas would make `/ship` mean "do stuff", which is exactly what `/target` already is. Route areas through `/target` or `/megawalk`. Types with a plausible-but-unwired green (`gtm` / launch) are post-MVP and rejected until each has a defined green.

## Vocabulary: "ship" the verb vs the ship phase/gate

`/ship` (this verb) = drive a deliverable to its finish line. It is distinct from the *ship phase* and *ship gate* inside `/target`, from the `DonePRGreen`/`DoneAdvisory` termination reasons, from `fno pr merge`, and from `/ship-docs` (which generates documentation and is NOT a ship type). The single canonical disambiguation lives in `AGENTS.md` -> "Ship vocabulary"; read it if the overlap is confusing.

## Composition, not self-containment

`/ship` is a **composing umbrella**, deliberately NOT a self-contained, liftable-in-isolation skill. `/ship pr` routes to the co-installed `/pr` skill (which stays the real implementation and permanent alias - the plan's no-forced-migration rule); `/pr` is therefore a hard companion dependency, not reimplemented here. Only the genuinely-new `doc` mode is local to this folder ([doc.md](references/doc.md), loaded via Read). This skill is intentionally excluded from the marketplace self-containment lint, because folding `/pr`'s ~400 lines of mode bodies in would tax the dominant code path for no payoff.

## Step 1: Resolve the type (ALWAYS announce it)

This is a **router**, not a monolith. Parse the first argument token:

- **no argument** -> do NOT default and do NOT guess. Print the menu and stop with a non-zero result:

  ```
  /ship needs a type. valid types:
    pr     drive a PR through its lifecycle (create | check | merged)
    doc    ship a research brief to output_dir and grade it
  ```

- **`pr`** -> the PR lifecycle. Print `running ship pr (PR lifecycle)`. The remaining tokens are the `/pr` mode + its arguments. Defer to the `/pr` skill: run `/pr <remaining tokens>` and follow it. (`/pr` is the retained, permanent alias - `/pr create` and `/ship pr create` are the same thing, byte-for-byte. `/ship pr` does not reimplement the PR flow; it routes to the one implementation in the `pr` skill.)
- **`doc`** or **`artifact`** -> the research-doc deliverable. Print `running ship doc (research brief + grade)`. Load [doc.md](references/doc.md) and execute it in full in this context. The remaining tokens are doc's arguments.
- **`budget`** or **`community`** -> NOT a ship type. Print and stop with a non-zero result:

  ```
  '<token>' is not a ship type: it is an ongoing area with no mechanical finish line.
  Route it through /target (one feature) or /megawalk (a backlog of them).
  ```

- **`gtm`** or **`launch`** -> a plausible deliverable, but its green is not wired yet (post-MVP). Print and stop with a non-zero result:

  ```
  'gtm' has no defined green yet (published + metric threshold) - post-MVP.
  Until then, drive launch work through /target.
  ```

- **any other non-empty token** -> unknown type (likely a typo). Do NOT default, do NOT guess. Print:

  ```
  unknown ship type: '<token>'
  valid types: pr, doc (no default - pick a deliverable)
  ```

  and stop with a non-zero result. This is the locked router contract: an unknown or empty type never silently falls through to an action.

## Multi-CLI

Claude-Code primary. `ship pr` needs everything `/pr` needs (`fno`, `gh`, `git`, a Haiku-capable provider for `create`). `ship doc` needs `fno` (the `research` + `evals grade` verbs). If a dependency is missing, the type fails loud and reports it - it never fakes a PR, a brief, or a grade.

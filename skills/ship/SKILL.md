---
name: ship
description: Open or create a pull request, check reviews, or ship a research brief through delivery gates.
argument-hint: "<pr|doc>  (pr: create|check|merged - the PR lifecycle; doc: <topic> [--golden <discovery-*.md>])  - a type is required, there is no default"
metadata:
  requires:
    binaries:
      - "fno >= 0.1"
      - "gh >= 2.0"
      - "git >= 2.0"
---

# Ship

**One verb for delivering anything.** `/ship <type>` drives a deliverable to its finish line, dispatching on the *deliverable type* the way `/target` dispatches on task type. `pr` only names the code branch. `/ship` names the whole family.

| Type | Finish line (the mechanical "green") | What runs |
|------|--------------------------------------|-----------|
| `pr` | PR exists + CI green + required bot reviewed, no unaddressed blocking finding (`DonePRGreen`) | the `pr` router ([pr.md](references/pr.md); `create` / `check` / `merged`) |
| `doc` (alias `artifact`) | brief written to `config.research.output_dir` + `fno doctor evals grade` green (`DoneAdvisory`) | [doc.md](references/doc.md), in this same context |

## The membership test (load-bearing)

A thing is a ship type ONLY if it has a definable **green** - a finish line readable mechanically. `pr` and `doc` both do. An ongoing *area* (`budget`, `community`) has no crisp green: it never "finishes", so it is not a deliverable. Admitting areas would make `/ship` mean "do stuff", which is exactly what `/target` already is. Route areas through `/target`. Types with a plausible-but-unwired green (`gtm` / launch) are post-MVP and rejected until each has a defined green.

## Vocabulary: "ship" the verb vs the ship phase/gate

`/ship` (this verb) = drive a deliverable to its finish line. It is distinct from the *ship phase* and *ship gate* inside `/target`, from the `DonePRGreen`/`DoneAdvisory` termination reasons, from `fno do pr merge`, and from `/ship-docs` (which generates documentation and is NOT a ship type). The single canonical disambiguation lives in `AGENTS.md` -> "Ship vocabulary"; read it if the overlap is confusing.

One owner decides every merge. `fno do pr merge` asks it, and so do `fno do pr verify --kind merged` and the `fno-agents finalize` queue arm. It answers merge or arm for one exact head, and it never emits an unpinned request. See [authorized-merge](../../docs/architecture/authorized-merge.md).

## Self-contained

Both mode bodies are local to this folder. The PR lifecycle router is [pr.md](references/pr.md), with `create.md`, `check.md`, `merged.md`, and `scripts/` beside it. The doc deliverable is [doc.md](references/doc.md). Each loads via Read. The former top-level `/fno:pr` skill retired into `references/pr.md` on 2026-09-30. `/fno:ship pr` is the one spelling and no alias survives.

## Step 1: Resolve the type (ALWAYS announce it)

This is a **router**, not a monolith. Parse the first argument token:

- **no argument** -> do NOT default and do NOT guess. Print the menu and stop with a non-zero result:

  ```
  /ship needs a type. valid types:
    pr     drive a PR through its lifecycle (create | check | merged)
    doc    ship a research brief to output_dir and grade it
  ```

- **`pr`** -> the PR lifecycle. Print `running ship pr (PR lifecycle)`. The remaining tokens are the pr mode + its arguments. Load [pr.md](references/pr.md) and execute it in this same context. It resolves the mode (`create` / `check` / `merged`) and runs the matching body here.
- **`doc`** or **`artifact`** -> the research-doc deliverable. Print `running ship doc (research brief + grade)`. Load [doc.md](references/doc.md) and execute it in full in this context. The remaining tokens are doc's arguments.
- **`budget`** or **`community`** -> NOT a ship type. Print and stop with a non-zero result:

  ```
  '<token>' is not a ship type: it is an ongoing area with no mechanical finish line.
  Route it through /target (one feature at a time).
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

## Known Limitations and Deferred Work

- Merge consent and review must hold at merge time. See [LIMITATIONS.md](LIMITATIONS.md).

## Multi-CLI

Claude-Code primary. `ship pr` needs `fno`, `gh`, `git`, and the `create` worker from the configured role routing (the `pr` router owns that contract). `ship doc` needs `fno` (the `research` + `evals grade` verbs). If a dependency is missing, the type fails loud and reports it - it never fakes a PR, a brief, or a grade.

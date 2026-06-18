---
claims: ab-test-001
created: 2026-05-18
status: ready
type: think
feature: sample plan for brief generation tests
---

# Sample Plan for Brief Generation Tests

## Overview

This is the first paragraph of the overview section. It describes a small sample feature for testing the brief generation command. The overview gives workers a single-sentence project context before diving into task details.

## Architecture

The architecture uses a simple two-file layout with a library module and a CLI entry point.

## User Stories

**US1:** As a developer, I want to run the sample command and see output.

**US2:** As an operator, I want a scoped brief for a specific task.

## Failure Modes

**Boundaries**
- The system must reject missing plan files with exit code 1
- The system must reject unknown task-ids with exit code 2

**Errors**
- The system must surface YAML parse failures with exit code 3

## Acceptance Criteria

**AC2-HP:** Given a plan with task 2.1, when I call brief, then I get a markdown brief with project context, task spec, ACs, files, and verify command.

**AC2-ERR:** Given an unknown task-id, when I call brief, then exit code is 2 and stderr lists valid task-ids.

**AC2-UI:** With --format json, output matches the fixed schema.

**AC2-EDGE:** Given no tagged entries, all Locked Decisions are included (fail-open).

**AC2-FR:** Given malformed Execution Strategy YAML, exit code is 3.

## Locked Decisions (DO NOT revisit)

1. **Use stdlib json.** Use the stdlib json module, not orjson. *Why:* zero extra dependency, sufficient for the brief use case. *How to apply:* import json; call json.dumps.

2. **Fail-open on untagged entries.** Entries without surface tags are included by default. *Why:* avoids silent brief shrinkage. *How to apply:* if no tag match, include the entry.

## Patterns to Reuse

| Pattern | Source | Why reuse |
|---|---|---|
| `resolve_repo_root()` | fno.paths | path anchoring from any cwd |
| TyperRunner | cli/src/fno/plan/brief.py | consistent CliRunner pattern |

## Execution Strategy

> Bootstrapped by hand for testing.

```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    name: "Foundation"
    tasks: [1.1]
  - wave: 2
    mode: sequential
    name: "Surface"
    tasks: [2.1]

tasks:
  - id: "1.1"
    title: "Foundation module"
    surface:
      - cli/src/fno/sample/core.py
      - cli/tests/unit/sample/test_core.py
    verify: "uv run pytest cli/tests/unit/sample/test_core.py -v"
    acceptance: [AC2-HP]
    notes: |
      Build the core module.

  - id: "2.1"
    title: "CLI entry point"
    surface:
      - cli/src/fno/plan/brief.py
      - cli/tests/unit/plan/test_brief.py
    verify: "uv run pytest cli/tests/unit/plan/test_brief.py -v"
    acceptance: [AC2-HP, AC2-ERR, AC2-UI, AC2-EDGE, AC2-FR]
    notes: |
      Build the CLI verb for brief generation.
```

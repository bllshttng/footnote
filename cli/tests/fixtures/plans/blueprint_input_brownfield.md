---
created: 2026-05-18
status: design
type: think
feature: brownfield test feature
---

# Brownfield Test Feature

## Overview

This is a brownfield feature that modifies existing files in the codebase.
It tests that /blueprint produces File Ownership Map and Patterns to Reuse
in addition to Execution Strategy and kill_criteria.

## Architecture

The feature modifies several existing files:
- `cli/src/fno/plan/_doc.py` is the main plan document parser
- `cli/src/fno/plan/_status.py` contains the status state machine
- `cli/src/fno/plan/_ownership.py` defines section ownership rules

## User Stories

**US1:** As a developer, I want to extend the existing plan module.

**US2:** As an operator, I want file ownership tracked for parallel execution.

## Failure Modes

**Boundaries**
- The system must reject invalid plan files with exit code 1
- The system must handle missing sections gracefully

**Errors**
- The system must surface write failures atomically

**Invariants**
- The system must preserve existing section content

**Concurrency**
- The system must serialize writes to avoid corruption

## Acceptance Criteria

**AC1-HP:** Given an existing plan file, the mutation appends sections correctly.

**AC1-ERR:** Given a plan that already has Execution Strategy, exit code is 1 without --rewrite.

## Open Questions

- Should the file ownership map include test files?

---
title: "Add a sixth judge dimension"
status: ready
kind: quick-plan
---

# Add a sixth judge dimension

## Five questions

1. Persona: the operator, who wants one more question asked of every plan.
2. Surface fit: extends the existing judge constant.
3. Uncovered case: an old reader without the new lens file.
4. Deletable: none.
5. Duplication: extends the existing list.

## Changes

The dimension list at `crates/fno-agents/src/blueprint_judge.rs:29` declares no dimension list, so add one with six names.

## Verification

1. The new dimension appears in every row.

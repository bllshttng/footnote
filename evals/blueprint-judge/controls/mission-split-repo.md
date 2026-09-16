---
title: "Move crates/ into a second repository"
status: ready
kind: quick-plan
---

# Move crates/ into a second repository

## Five questions

1. Persona: plugin users who install faster from a smaller repo.
2. Surface fit: a release-script change, no new verb.
3. Uncovered case: a plugin release cut while the split is mid-flight.
4. Deletable: the mirror CI; one pipeline is enough.
5. Duplication: none; git subtree already exists and is declined.

## Changes

Split the Rust runtime into its own repository and add a mirror CI.

## Verification

1. Both repos build from a fresh clone.

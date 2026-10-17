---
status: ready
kind: quick-plan
created: 2026-10-17
---

# Retire the unchecked cache read

## Context

`cache.read_any` skips the etag check, so `status.py` can act on a stale row. A caller that moves to `cache.read(key, etag)` takes a new `etag` argument after `n` (or after `prs` in a list caller).

## Routes

1. Move `status.py` only and keep `read_any` for the rest - ruled out by: any other module that calls `read_any`.
2. Move every caller and delete `read_any` - ruled out by: a caller that needs the unchecked read.

## Unknowns

- [ ] which modules call `read_any`, under any import form - measure: read every `.py` file in this directory

## Done

`read_any` has zero definitions and zero call sites in this directory.

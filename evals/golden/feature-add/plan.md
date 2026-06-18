---
status: ready
---
# Add slugify to textkit

## Goal

Add a `slugify(text)` function to `textkit.py` that converts a string to a
URL-friendly slug: lowercase, spaces and punctuation replaced by single
hyphens, leading/trailing hyphens trimmed.

## Tasks

1. Implement `slugify(text: str) -> str` in `textkit.py`.
2. Add tests for `slugify` to `test_textkit.py` covering at least: basic
   lowercasing + space-to-hyphen, punctuation collapsing, leading/trailing
   hyphen trimming.
3. Ensure the full test suite passes.

## Acceptance Criteria

- `slugify("Hello World")` returns `"hello-world"`.
- `slugify("Python 3.11!")` returns `"python-3-11"`.
- `slugify("  --trim me--  ")` returns `"trim-me"`.
- All existing tests still pass.

"""Retro-triage: harvest left-out work for a merged PR into backlog nodes.

Pipeline: harvest -> classify -> dedup -> land. Each stage is a pure-ish module
(deterministic, injectable IO) so the whole routine is unit-testable without a
live `gh` or graph. The routine is hosted at an LLM-present checkpoint (the
`/target` post-merge fast-path or the retro-sentinel consumer) but never invents
a finding: every node carries its source's reasoning verbatim plus a source cite.
"""
from __future__ import annotations

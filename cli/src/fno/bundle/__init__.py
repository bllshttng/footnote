"""fno bundle subcommands - thin wrappers over canonical bundler scripts.

Surface:
    fno bundle         - regenerate per-skill bundles from skill-bundles.yaml
                         (default action when no subcommand given)
    fno bundle check   - verify committed bundles match canonical (freshness gate)
    fno bundle lint    - run the marketplace-readiness lint on driver skills

Each invocation is a thin Typer wrapper that forwards to the canonical bash
script under scripts/. The bash scripts remain the single source of truth;
this CLI exists for discoverability (`fno --help`) and so contributors, the
pre-commit hook, and CI all converge on the same surface.

Module path is ``fno.bundle`` (not ``fno.skills``) so the
``skills`` namespace stays free for future content management (e.g.
``fno skills list``). The verb operates on the bundle manifest, not on
the skills themselves.
"""
from fno.bundle.cli import bundle_app

__all__ = ["bundle_app"]

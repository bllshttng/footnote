"""Every remedy verb an rm or spawn refusal names must parse at the front door.

A refusal that names a verb the CLI does not have sends its reader to a dead
end: rm once told operators to `rotate` a blocked row, and no such verb exists.
"""
from __future__ import annotations

import re
from pathlib import Path

import click
import typer.main

from fno.agents.cli import agents_app
from fno.agents.spawn_defaults import SEEDLESS_THREAD_REFUSAL

REPO = Path(__file__).resolve().parents[3]
VERB_RE = re.compile(r"fno agents ([a-z][a-z0-9-]*)")


def _resolves(verb: str) -> bool:
    # The real agents group: it serves Python commands and synthesizes the
    # Rust-only verbs (adopt, reap, ...), so one call covers both runtimes.
    group = typer.main.get_command(agents_app)
    ctx = click.Context(group, info_name="agents")
    return group.get_command(ctx, verb) is not None


def _named_verbs() -> set[str]:
    sources = [
        (REPO / "crates/fno-agents/src/daemon/rm_refusal_detail.rs").read_text(),
        (REPO / "cli/src/fno/agents/rm_notice.py").read_text(),
        SEEDLESS_THREAD_REFUSAL,
    ]
    return {m.group(1) for text in sources for m in VERB_RE.finditer(text)}


def test_every_remedy_verb_named_by_an_rm_or_spawn_refusal_parses():
    verbs = _named_verbs()
    # Positive control: the scan read the sources it claims to read.
    assert {"stop", "adopt", "spawn"} <= verbs, verbs
    unresolved = sorted(v for v in verbs if not _resolves(v))
    assert unresolved == [], f"refusal text names verbs the CLI lacks: {unresolved}"


def test_the_resolver_rejects_a_verb_the_cli_does_not_have():
    # Negative control: the call that clears the named verbs refuses rotate.
    assert not _resolves("rotate")
    assert _resolves("stop") and _resolves("adopt")

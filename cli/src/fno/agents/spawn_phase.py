"""The spawn's semantic phase label: which verb does the message name?

A phase stamps the node's sessions[] row and follows the assignment for
life - a review row is a retirement blocker the row never grows out of - so
the label comes from the verb table (spawn_phase.toml, authored in the Rust
tree, shipped here as package data), never from a mangled prefix chain.
Explicit --session-phase wins before this helper runs; when the inference
answers "" on a --node spawn, cmd_spawn refuses with exit 2 and no worker
launches (x-007c).
"""

from __future__ import annotations

from functools import cache

from fno.config._dispatch_verbs import parse_verb_token


@cache
def _verb_phases() -> dict[str, str]:
    """The verb-to-phase vocabulary from the ONE canonical spawn_phase table
    (x-007c): authored in the Rust tree, shipped here as generated package
    data. The Python reader and any future Rust reader cannot drift."""
    import tomllib
    from importlib.resources import files

    table = tomllib.loads(
        files("fno.agents").joinpath("spawn_phase.toml").read_text(encoding="utf-8")
    )
    phases: dict[str, str] = {}
    for phase, verbs in table["phases"].items():
        for verb in verbs:
            phases[verb] = phase
    return phases


def infer_phase(message: str | None) -> str:
    """do | review | blueprint | think | ship | "" when unlabelable.

    First whitespace token, parsed through the one verb-token owner; the
    verb word looks up in the table. Anything else (prose, unmapped verbs,
    empty) answers ""."""
    verb = (message or "").lstrip().split(maxsplit=1)[0] if message else ""
    parsed = parse_verb_token(verb) if verb else None
    return _verb_phases().get(parsed[0], "") if parsed else ""

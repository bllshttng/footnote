"""Every front argv the Rust checkin/loop emitters pass resolves for real.

x-cf9e: the rename to `fno agents org` left the Rust beat spawning
`["agents", "lead", "drain", ...]`, a spelling the front refuses, so one
reader failed each beat. The argv literals are pinned from the two Rust
emitters' source (the house form, as daemon.rs's mcp pin), the drain argv
runs through the real door dispatcher, and the dead `lead` spelling is the
negative control.
"""
from __future__ import annotations

import re
from pathlib import Path

from typer.testing import CliRunner

REPO_ROOT = Path(__file__).resolve().parents[3]
EMITTERS = [
    REPO_ROOT / "crates/fno-agents/src/lead_checkin.rs",
    REPO_ROOT / "crates/fno-agents/src/loop_lead.rs",
]

runner = CliRunner()


def _front_argvs() -> list[list[str]]:
    argvs: list[list[str]] = []
    for path in EMITTERS:
        for m in re.finditer(r'\[\s*"agents"[^\]]*\]', path.read_text(encoding="utf-8")):
            tokens = re.findall(r'"([^"]*)"', m.group(0))
            if len(tokens) >= 2:
                argvs.append(tokens)
    assert len(argvs) >= 3, "the emitters moved; teach this test their new shape"
    return argvs


def test_every_emitted_argv_is_an_org_spelling():
    argvs = _front_argvs()
    assert not any("lead" in argv for argv in argvs), argvs
    assert ["agents", "org", "drain"] in argvs

    stale = [
        str(p)
        for base in ("src", "tests")
        for p in (REPO_ROOT / "crates/fno-agents" / base).rglob("*.rs")
        if re.search(r'"agents",\s*"lead"', p.read_text(encoding="utf-8"))
    ]
    assert not stale, f"dead lead spellings remain: {stale}"


def test_the_drain_argv_dispatches_and_lead_refuses():
    from fno.agents.cli import agents_app

    # The door execs the binary; drain is a count read, so the dispatch is
    # safe against any graph it finds. Only "No such command" is the failure
    # the rename shipped; a graph error still proves the spelling resolved.
    result = runner.invoke(agents_app, ["org", "drain"])
    combined = (result.output or "") + (result.stderr or "")
    assert "No such command" not in combined, combined

    dead = runner.invoke(agents_app, ["lead", "drain"])
    combined = (dead.output or "") + (dead.stderr or "")
    assert dead.exit_code != 0
    assert "No such command" in combined

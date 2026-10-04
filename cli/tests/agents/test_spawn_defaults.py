"""The spawn seam's end-to-end contract over the real compose.

The composition is characterized by the Rust goldens
(crates/fno-agents/tests/fixtures/spawn_compose/); the seam-visibility tests
that walked the old Python body live there now. What stays here is the path
no golden can pin: a real `fno-agents` binary answering the transport's one
verb call under a pinned FNO_CONFIG.
"""
from __future__ import annotations

import io
import json
from pathlib import Path

import pytest

import fno.agents.spawn_defaults as sd

requires_rust = pytest.mark.dev_build


def test_mint_node_name_is_the_source_less_manual_t_form(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A manual --node spawn carries NO source segment: provenance is stamped
    only by the path that knows it. The model rides as the mint's final
    segment over the bare node hex (x-57fe)."""
    monkeypatch.setattr(
        sd, "_node_slug_from_graph", lambda node: ("x-84b2", "Ab Names")
    )
    name = sd._mint_node_name("x-84b2", None, "glm-5.3-flash")
    assert name == "t-84b2-ab-names-glm"


def test_retired_bg_substrate_refuses_with_the_redirect(capsys):
    """The bg spelling is retired, not deprecated: one line, the replacement,
    exit 2. thread keeps working and canonicalizes to the internal selector."""
    import pytest

    from fno.agents.spawn_defaults import resolve_spawn_gates

    with pytest.raises(SystemExit) as e:
        resolve_spawn_gates("bg", None, once=False, harness="claude")
    assert e.value.code == 2
    assert "substrate 'bg' was retired" in capsys.readouterr().err
    assert resolve_spawn_gates("thread", None, once=False, harness="claude") == "bg"


def _pin_world(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, config: str) -> Path:
    """The real path: this checkout's binary, one config, one journal."""
    from fno.rust_binary import find_dev_binary

    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    monkeypatch.setenv("FNO_AGENTS_BIN", str(binary))
    config_path = tmp_path / "config.toml"
    config_path.write_text(config, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(config_path))
    journal = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(journal))
    monkeypatch.setenv("FNO_NO_CANONICAL_CONFIG", "1")
    return journal


@requires_rust
def test_config_provider_injects_the_harness_end_to_end(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """One config field, one binary round trip: the argv carries --harness,
    stderr is quiet about the routing by default and names the applied axis
    with its rung under --verbose, and the journal row lands."""
    journal = _pin_world(
        tmp_path, monkeypatch, '[agents.profiles.target]\nprovider = "codex"\n'
    )
    err = io.StringIO()
    out = sd.compose_spawn_argv(
        ["spawn", "--name", "w", "/fno:target x-1"], stderr=err
    )
    assert out[out.index("--harness") + 1] == "codex"
    printed = err.getvalue()
    assert ": applied " not in printed, printed
    assert len([l for l in printed.splitlines() if l.strip()]) <= 2, printed
    rows = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
    applied = [r for r in rows if r.get("kind") == "spawn_defaults_applied"]
    assert applied, "exactly the compose wrote the row"
    assert applied[-1]["verb"] == "target"
    err = io.StringIO()
    sd.compose_spawn_argv(
        ["spawn", "--name", "w", "--verbose", "/fno:target x-1"], stderr=err
    )
    assert "applied harness=codex (agents.profiles.target.provider)" in err.getvalue()


@requires_rust
def test_transport_names_the_skip_when_the_verb_is_unavailable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No binary: one named skip line, the normalized argv, no raise. The
    resolver falls through a bogus FNO_AGENTS_BIN by design (a stale export
    must not strand the binary), so the fault is raised at the transport."""
    from fno.agents import spawn_overlay_client

    def boom(payload, *a, **k):
        raise spawn_overlay_client.SpawnOverlayUnavailable("no binary")

    monkeypatch.setattr(
        spawn_overlay_client, "spawn_overlay_call", boom
    )
    err = io.StringIO()
    out = sd.compose_spawn_argv(
        ["spawn", "--name", "w", "/fno:target x-1"], stderr=err
    )
    assert "config defaults skipped (spawn-overlay unavailable: no binary)" in err.getvalue()
    assert "--name" in out

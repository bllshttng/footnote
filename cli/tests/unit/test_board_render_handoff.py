"""The render-views -> front-binary handoff, against a stub binary.

The handoff's own contract: the request carries EXPANDED target paths (the
config rows are operator text with `~` in them), a receipt failure counts,
and an install whose front binary predates the verb degrades to one named
skip instead of a failed render.
"""
from __future__ import annotations

import json
import stat
import subprocess
from pathlib import Path

import pytest

from fno.config import RenderTargetConfig
from fno.graph import roadmap_public as rp

# The root conftest stubs render_local_targets for every test; these ARE the
# render's tests, so they restore the import-time real.
_REAL_RENDER = rp.render_local_targets


def _stub_binary(tmp_path: Path, receipt: dict | None, *, old_verb: bool = False) -> Path:
    stub = tmp_path / "stub-front"
    if old_verb:
        body = "#!/bin/sh\necho \"Error: No such command 'board-render'.\" >&2\nexit 1\n"
    else:
        payload = json.dumps(receipt or {}).replace("'", "'\\''")
        body = f"#!/bin/sh\nprintf '%s' '{payload}'\n"
    stub.write_text(body, encoding="utf-8")
    stub.chmod(stub.stat().st_mode | stat.S_IEXEC)
    return stub


@pytest.fixture
def targets(tmp_path: Path) -> list[RenderTargetConfig]:
    return [
        RenderTargetConfig(
            path="~/f0f6-tilde-probe/board.html",
            scope=rp.ALL_PROJECTS,
            projection="local",
        )
    ]


def test_the_request_carries_expanded_paths_and_counts_receipt_failures(
    tmp_path: Path, monkeypatch, targets: list[RenderTargetConfig]
):
    stub = _stub_binary(tmp_path, {"written": [], "failed": [{"path": "y", "error": "boom"}]})
    seen: dict = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["request"] = json.loads(kwargs["input"])
        receipt = json.dumps({"written": [], "failed": [{"path": "y", "error": "boom"}]})
        return subprocess.CompletedProcess(argv, 0, stdout=receipt, stderr="")

    monkeypatch.setattr(rp, "render_local_targets", _REAL_RENDER)
    monkeypatch.setattr("fno.rust_binary.resolve_front_binary", lambda: stub)
    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(rp, "_configured_targets", lambda: targets)
    monkeypatch.setattr(rp, "_load_obsidian_vault", lambda: "c3po")

    assert rp.render_local_targets() == 1

    request = seen["request"]
    assert request["targets"][0]["path"] == str(Path.home() / "f0f6-tilde-probe/board.html")
    assert request["targets"][0]["scope"] == rp.ALL_PROJECTS
    assert request["vault"] == "c3po"


def test_an_install_predating_the_verb_skips_named_not_failed(
    tmp_path: Path, monkeypatch, targets: list[RenderTargetConfig], capsys
):
    stub = _stub_binary(tmp_path, None, old_verb=True)

    def refusing_run(argv, **kwargs):
        return subprocess.CompletedProcess(argv, 1, stdout="", stderr="No such command 'board-render'.")

    monkeypatch.setattr(rp, "render_local_targets", _REAL_RENDER)
    monkeypatch.setattr("fno.rust_binary.resolve_front_binary", lambda: stub)
    monkeypatch.setattr(subprocess, "run", refusing_run)
    monkeypatch.setattr(rp, "_configured_targets", lambda: targets)
    monkeypatch.setattr(rp, "_load_obsidian_vault", lambda: None)

    assert rp.render_local_targets() == 0
    assert "predates board-render" in capsys.readouterr().err

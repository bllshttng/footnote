"""One harness axis in `_spawn_worker`, and a receipt for every spawn (x-374b).

The specimen: an auto-continue dispatch launched a claude worker whose first
user line was `$fno:target x-30c2`, the codex spelling. Two values were computed
from two sources - the COMMAND surface came from `resolve_dispatch`, which reads
the stage table, while the LAUNCH binary came from `provider or "claude"`. These
tests pin them to one resolve.

No test spawns: `advance.subprocess.run` is mocked and the receipt is read back
from an isolated events path.
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.backlog import advance


def _settings(*, stage_harness: str = "", legacy_harness: str = ""):
    """A settings stub whose stage table names `stage_harness` for /target."""
    profile = SimpleNamespace(provider=stage_harness)
    return SimpleNamespace(
        agents=SimpleNamespace(
            profiles={"target": profile} if stage_harness else {},
            spawn_permission_mode="",
        ),
        dispatch=SimpleNamespace(
            harness=legacy_harness, substrate="", command="", allowed_verbs=[]
        ),
        auto_merge=SimpleNamespace(grant=None),
    )


def _capture(monkeypatch, settings):
    """Mock the spawn subprocess; return the dict holding the argv sent."""
    captured: dict = {}

    def fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        # A full session id, not a head-8: a bare 8-hex aimed at codex is a
        # 65.5-second timestamp bucket and the spawn seam refuses it by shape.
        return SimpleNamespace(
            returncode=0,
            stdout='{"name":"w","session_id":"0de85539-1a2b-7c3d-8e4f-5a6b7c8d9e0f"}',
            stderr="",
        )

    monkeypatch.setattr(advance.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    # The grid is a separate axis; keep it out of these argv assertions.
    monkeypatch.setattr(
        advance, "_grid_lane_for", lambda node, **kw: (None, None, "grid=test-stub")
    )
    return captured


def _flag(cmd, name):
    return cmd[cmd.index(name) + 1]


def _message(cmd):
    """The dispatched command is the last argv element."""
    return cmd[-1]


# --- the argv pair: one harness, one spelling ------------------------------


def test_stage_table_harness_drives_both_launch_and_command(monkeypatch):
    """The specimen. Stage table says codex; nothing is pinned.

    Before: `--harness claude` (from `provider or "claude"`) carrying a
    `$fno:target` message no claude worker can run.
    """
    captured = _capture(monkeypatch, _settings(stage_harness="codex"))
    advance._spawn_worker("x-0000", None, "slug")
    assert _flag(captured["cmd"], "--harness") == "codex"
    assert _message(captured["cmd"]).startswith("$fno:target")


def test_explicit_harness_pins_both(monkeypatch):
    """Same config, `harness="claude"` explicit: claude launch, claude spelling."""
    captured = _capture(monkeypatch, _settings(stage_harness="codex"))
    advance._spawn_worker("x-0000", None, "slug", harness="claude")
    assert _flag(captured["cmd"], "--harness") == "claude"
    assert _message(captured["cmd"]).startswith("/target")


def test_provider_pins_the_surface_not_only_the_launch(monkeypatch):
    """`provider` IS the harness axis here, so it must reach the resolver."""
    captured = _capture(monkeypatch, _settings(stage_harness="claude"))
    advance._spawn_worker("x-0000", None, "slug", provider="codex")
    assert _flag(captured["cmd"], "--harness") == "codex"
    assert _message(captured["cmd"]).startswith("$fno:target")


def test_no_config_falls_back_to_the_resolvers_builtin(monkeypatch):
    """Nothing set anywhere: the resolver owns the claude fallback, not `prov`."""
    captured = _capture(monkeypatch, _settings())
    advance._spawn_worker("x-0000", None, "slug")
    assert _flag(captured["cmd"], "--harness") == "claude"
    assert _message(captured["cmd"]).startswith("/target")


def test_launch_harness_disagreeing_with_the_surface_refuses(monkeypatch):
    """An explicit harness and an explicit, different provider is the split
    this node exists to close: refuse rather than ship a mismatched pair."""
    captured = _capture(monkeypatch, _settings())
    with pytest.raises(advance.SpawnError) as exc:
        advance._spawn_worker("x-0000", None, "slug", harness="claude", provider="codex")
    assert "harness" in str(exc.value)
    assert "cmd" not in captured, "refused before spawning"


# --- the receipt ------------------------------------------------------------


def _rows(events_path: Path, kind: str) -> list[dict]:
    if not events_path.exists():
        return []
    out = []
    for line in events_path.read_text().splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        if row.get("type") == kind:
            out.append(row)
    return out


def test_spawn_emits_one_dispatch_spawned_row(monkeypatch, tmp_path):
    """The receipt names the resolved pair, so the file distinguishes callers."""
    captured = _capture(monkeypatch, _settings(stage_harness="codex"))
    ev = tmp_path / "events.jsonl"
    advance._spawn_worker(
        "x-0000", None, "slug", caller="_converge_one", events_path=ev
    )
    rows = _rows(ev, "dispatch_spawned")
    assert len(rows) == 1
    data = rows[0]["data"]
    assert data["harness"] == _flag(captured["cmd"], "--harness")
    assert data["command"] == _message(captured["cmd"])
    assert data["caller"] == "_converge_one"
    assert data["node_id"] == "x-0000"
    assert data["substrate"] == _flag(captured["cmd"], "--substrate")
    assert data["grid"] == "grid=test-stub"
    assert data["account"] == ""


def test_receipt_names_the_pinned_account_record(monkeypatch, tmp_path):
    """A record whose config points at the wrong login makes every other field
    name a lane it did not bill, and only the record id makes that readable."""
    _capture(monkeypatch, _settings())
    ev = tmp_path / "events.jsonl"
    advance._spawn_worker(
        "x-0000", None, "slug", dispatch_account="ccr", events_path=ev
    )
    assert _rows(ev, "dispatch_spawned")[0]["data"]["account"] == "ccr"


def test_failed_spawn_emits_no_receipt(monkeypatch, tmp_path):
    """A receipt is proof of a launch, so a non-zero exit leaves none."""

    def fake_run(cmd, **kwargs):
        return SimpleNamespace(returncode=1, stdout="", stderr="boom")

    monkeypatch.setattr(advance.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.config.load_settings", lambda: _settings())
    monkeypatch.setattr(
        advance, "_grid_lane_for", lambda node, **kw: (None, None, None)
    )
    ev = tmp_path / "events.jsonl"
    with pytest.raises(advance.SpawnError):
        advance._spawn_worker("x-0000", None, "slug", events_path=ev)
    assert _rows(ev, "dispatch_spawned") == []

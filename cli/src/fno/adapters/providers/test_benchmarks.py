"""Tests for the OpenRouter benchmark snapshot + reachability mapping.

The network is never touched: refresh takes an ``opener`` injection seam, and a
``now``/``path`` seam keeps the snapshot deterministic and off the real
``~/.fno``. Covers the happy fetch, loud auth/network/429 failures, atomic
writes, snapshot validation, staleness, and reachability lookups.
"""
from __future__ import annotations

import io
import json
import urllib.error

import pytest

from fno.adapters.providers import benchmarks as bm

_PAYLOAD = {
    "models": [
        {"name": "claude-opus-4-8", "coding_percentile": 99},
        {"name": "glm-4.7", "coding_percentile": 71},
        {"name": "some-unmapped-model", "coding_percentile": 50},
    ]
}


def _opener(payload=_PAYLOAD):
    def _open(req, timeout=None):
        return io.BytesIO(json.dumps(payload).encode())

    return _open


def test_refresh_happy_writes_snapshot(tmp_path):
    dest = tmp_path / "benchmarks.json"
    snap = bm.refresh(
        path=dest, env={"OPENROUTER_API_KEY": "k"}, opener=_opener(), now=1_000_000.0
    )
    assert snap["source"] == bm.OPENROUTER_BENCHMARKS_URL
    assert snap["fetched_at"].startswith("1970")  # ts=1e6 -> 1970-01-12
    names = [m["name"] for m in snap["models"]]
    assert "claude-opus-4-8" in names and "glm-4.7" in names
    # on-disk file matches and is valid JSON (no truncation)
    on_disk = json.loads(dest.read_text())
    assert on_disk == snap


def test_refresh_no_key_fails_loud(tmp_path):
    dest = tmp_path / "benchmarks.json"
    with pytest.raises(bm.BenchmarkError) as ei:
        bm.refresh(path=dest, env={}, opener=_opener())
    assert "OPENROUTER_API_KEY" in str(ei.value)
    assert not dest.exists()  # nothing written on the auth-fail path


def test_refresh_429_fails_loud(tmp_path):
    def boom(req, timeout=None):
        raise urllib.error.HTTPError(req.full_url, 429, "Too Many Requests", {}, None)

    dest = tmp_path / "benchmarks.json"
    with pytest.raises(bm.BenchmarkError) as ei:
        bm.refresh(path=dest, env={"OPENROUTER_API_KEY": "k"}, opener=boom)
    assert "429" in str(ei.value)
    assert not dest.exists()


def test_refresh_network_error_leaves_no_file(tmp_path):
    def boom(req, timeout=None):
        raise urllib.error.URLError("connection refused")

    dest = tmp_path / "benchmarks.json"
    with pytest.raises(bm.BenchmarkError):
        bm.refresh(path=dest, env={"OPENROUTER_API_KEY": "k"}, opener=boom)
    assert not dest.exists()
    # no half-written temp file lingers either
    assert list(tmp_path.glob("benchmarks.json.tmp*")) == []


def test_refresh_empty_model_list_fails(tmp_path):
    dest = tmp_path / "benchmarks.json"
    with pytest.raises(bm.BenchmarkError):
        bm.refresh(
            path=dest, env={"OPENROUTER_API_KEY": "k"},
            opener=_opener({"models": []}),
        )
    assert not dest.exists()


def test_atomic_write_leaves_no_temp(tmp_path):
    dest = tmp_path / "benchmarks.json"
    bm.refresh(path=dest, env={"OPENROUTER_API_KEY": "k"}, opener=_opener(), now=1.0)
    assert dest.exists()
    assert list(tmp_path.glob("*.tmp*")) == []


def test_load_snapshot_absent_is_none(tmp_path):
    assert bm.load_snapshot(tmp_path / "nope.json") is None


def test_load_snapshot_missing_fields_is_none(tmp_path):
    dest = tmp_path / "benchmarks.json"
    dest.write_text(json.dumps({"models": []}))  # no fetched_at/source -> invalid
    assert bm.load_snapshot(dest) is None


def test_staleness_and_is_stale(tmp_path):
    dest = tmp_path / "benchmarks.json"
    snap = bm.refresh(path=dest, env={"OPENROUTER_API_KEY": "k"}, opener=_opener(), now=0.0)
    day = 86400
    assert not bm.is_stale(snap, now=1 * day)
    assert bm.is_stale(snap, now=20 * day)
    assert bm.staleness_seconds(snap, now=20 * day) == pytest.approx(20 * day)


def test_reachable_maps_known_and_rejects_unknown():
    assert bm.reachable("claude-opus-4-8") == ("claude", "claude-opus-4-8")
    assert bm.reachable("glm-4.7") == ("claude", "glm-4.7")
    # codex flagships route on the codex harness
    assert bm.reachable("gpt-5.5") == ("codex", "gpt-5.5")
    assert bm.reachable("gpt-5.4") == ("codex", "gpt-5.4")
    assert bm.reachable("some-unmapped-model") is None


def test_static_tiers_are_reachable():
    """Every curated static-tier name must be routable (else it is dead weight)."""
    for band, names in bm.STATIC_TIERS.items():
        for name in names:
            assert bm.reachable(name) is not None, f"{band} tier {name} is unmapped"


# --- CLI glue -------------------------------------------------------------- #


def _runner():
    from typer.testing import CliRunner

    from fno.adapters.providers.cli import cli

    return CliRunner(), cli


def test_cli_refresh_no_key_exits_1(monkeypatch):
    monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
    runner, cli = _runner()
    r = runner.invoke(cli, ["benchmarks", "refresh"])
    assert r.exit_code == 1
    assert "OPENROUTER_API_KEY" in r.output


def test_cli_show_no_snapshot_exits_1(monkeypatch):
    monkeypatch.setattr(bm, "load_snapshot", lambda path=None: None)
    runner, cli = _runner()
    r = runner.invoke(cli, ["benchmarks", "show"])
    assert r.exit_code == 1
    assert "no benchmark snapshot" in r.output


def test_cli_show_stale_warns(monkeypatch):
    stale = {"fetched_at": "1970-01-01T00:00:00+00:00", "source": "x",
             "models": [{"name": "glm-4.7", "coding_percentile": 71}]}
    monkeypatch.setattr(bm, "load_snapshot", lambda path=None: stale)
    runner, cli = _runner()
    r = runner.invoke(cli, ["benchmarks", "show"])
    assert r.exit_code == 0
    assert "days old" in r.output
    assert "glm-4.7" in r.output and "claude:glm-4.7" in r.output

"""Launch-account provenance: the receipt answers WHO chose the account.

The defect this pins: a spawn receipt named the account a worker launched on
but not WHO chose it, and a config injection (accounts.quota.pick_on_launch)
printed identically to a caller decision. The fix is one shared vocabulary -
"caller" / "config" (plus "env" / "default" where an axis realizes them) -
defined once in ``fno.agents.spawn_flag_owners`` and stamped by every spawn
seam onto the row (`launch_account_source`, registry v26) and the receipt
(`account_source`).

Run: cd cli && uv run pytest tests/unit/test_spawn_account_provenance.py -v
"""
from __future__ import annotations

import json
import time
from pathlib import Path

import pytest
import tomli_w
from typer.testing import CliRunner

from fno.agents import dispatch as dispatch_mod
from fno.agents import spawn_flag_owners


@pytest.fixture(autouse=True)
def _isolate_config_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The sanctioned test runner exports FNO_CONFIG; these fixtures own config."""
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "config.toml"))


def _write_config(tmp_path: Path, *, pick_on_launch: bool) -> None:
    alt, main = tmp_path / "claude-alt", tmp_path / "claude-main"
    for d in (alt, main):
        d.mkdir(exist_ok=True)
        (d / ".credentials.json").write_text(
            '{"claudeAiOauth":{"accessToken":"test-token","refreshToken":"test-refresh"}}'
        )
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "config.toml").write_text(
        tomli_w.dumps({
            "providers": {
                "active": "readyrule",
                "quota": {"pick_on_launch": pick_on_launch},
                "records": [
                    {"id": "readyrule", "name": "readyrule", "harness": "claude",
                     "auth": "managed", "config_dir": str(alt)},
                    {"id": "makers", "name": "makers", "harness": "claude",
                     "auth": "managed", "config_dir": str(main)},
                ],
            }
        }),
        encoding="utf-8",
    )


def _pin_config(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "config.toml"))


@pytest.fixture()
def armed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """readyrule exhausted, makers healthy, picking armed."""
    _write_config(tmp_path, pick_on_launch=True)
    _pin_config(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", str(tmp_path / "runtime-state.json"))

    from fno.adapters.providers.runtime_state import write_usage_snapshot
    from fno.adapters.providers.usage import UsageSnapshot, UsageWindow

    now = time.time()
    write_usage_snapshot(
        UsageSnapshot("readyrule", (UsageWindow("5h", 100.0, now + 3600),), now, "t"),
        now=now,
    )
    write_usage_snapshot(
        UsageSnapshot("makers", (UsageWindow("5h", 32.0, now + 3600),), now, "t"),
        now=now,
    )
    return tmp_path


def _stub_created(monkeypatch: pytest.MonkeyPatch, seen: dict) -> None:
    class _Created:
        short_id = "abc123"

    monkeypatch.setattr(
        dispatch_mod, "_claude_create_path",
        lambda **kw: (seen.update(kw), _Created())[1],
    )
    monkeypatch.setattr(dispatch_mod, "_emit_ev", lambda *a, **k: None)


def _run_bg_spawn(name: str, tmp_path: Path, **kw):
    monkeypatch_kw = kw.pop("monkeypatch")
    monkeypatch_kw.setenv("FNO_SPAWN_GATE", "0")
    from fno.agents.spawn_gate import run_gate

    gate = run_gate(name, "bg")
    try:
        return dispatch_mod.dispatch_spawn(
            name=name, message="hi", harness="claude", cwd=tmp_path, **kw
        )
    finally:
        gate.release()


class TestBgSeam:
    def test_bg_pick_names_config_as_the_source(
        self, armed: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        seen: dict = {}
        _stub_created(monkeypatch, seen)
        result = _run_bg_spawn("w-prov", armed, monkeypatch=monkeypatch)
        assert result.launch_account == "makers"
        assert result.launch_account_source == spawn_flag_owners.CONFIG
        assert seen["launch_account"] == "makers"

    def test_bg_explicit_account_names_caller(
        self, armed: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        seen: dict = {}
        _stub_created(monkeypatch, seen)
        # A caller's explicit account always arrives WITH its env overlay
        # (resolved at the CLI layer); the overlay is what suppresses the pick.
        result = _run_bg_spawn(
            "w-prov2", armed, monkeypatch=monkeypatch,
            launch_account="readyrule", account_env={"CLAUDE_CONFIG_DIR": "x"},
        )
        assert result.launch_account == "readyrule"
        assert result.launch_account_source == spawn_flag_owners.CALLER

    def test_bg_no_account_positively_defaults(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """No pick fired and no flag: launch_account is the "default" sentinel
        and the source stays None - launch_account already says nobody chose."""
        _write_config(tmp_path, pick_on_launch=False)
        _pin_config(tmp_path, monkeypatch)
        seen: dict = {}
        _stub_created(monkeypatch, seen)
        result = _run_bg_spawn("w-prov3", tmp_path, monkeypatch=monkeypatch)
        assert result.launch_account == "default"
        assert result.launch_account_source is None


class TestReceipts:
    """The receipt branch in cmd_spawn, exercised with stubbed back halves."""

    @staticmethod
    def _receipt_of(output: str) -> dict:
        """The JSON receipt line; the canonical-cwd note may follow it."""
        for line in output.strip().splitlines():
            if line[:1] == "{":
                return json.loads(line)
        raise AssertionError(f"no JSON receipt in output: {output!r}")

    @staticmethod
    def _stub_pane_back(monkeypatch: pytest.MonkeyPatch, result) -> None:
        from fno.agents import mux_spawn, spawn_gate

        class Gate:
            def release(self):
                pass

        monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: Gate())
        monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *a, **k: None)
        monkeypatch.setattr(
            mux_spawn, "dispatch_spawn_bounded_pane", lambda **kw: result
        )

    def test_pane_receipt_carries_account_and_source(
        self, armed: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.agents import mux_spawn
        from fno.agents.cli import agents_app

        self._stub_pane_back(
            monkeypatch,
            mux_spawn.MuxSpawnResult(
                name="p-prov", provider="claude", session="s", pane_id=1,
                child_pid=None, session_uuid=None,
                launch_account="readyrule", launch_account_source="caller",
            ),
        )
        result = CliRunner().invoke(
            agents_app, ["spawn", "--name", "p-prov", "hi", "--harness", "claude"]
        )
        assert result.exit_code == 0, result.output
        receipt = self._receipt_of(result.output)
        assert receipt["account"] == "readyrule"
        assert receipt["account_source"] == "caller"

    def test_pane_receipt_stays_quiet_without_an_account(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from fno.agents import mux_spawn
        from fno.agents.cli import agents_app

        self._stub_pane_back(
            monkeypatch,
            mux_spawn.MuxSpawnResult(
                name="p-quiet", provider="claude", session="s", pane_id=1,
                child_pid=None, session_uuid=None,
            ),
        )
        result = CliRunner().invoke(
            agents_app, ["spawn", "--name", "p-quiet", "hi", "--harness", "claude"]
        )
        assert result.exit_code == 0, result.output
        receipt = self._receipt_of(result.output)
        assert "account" not in receipt
        assert "account_source" not in receipt

    def test_bg_receipt_carries_config_pick(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        from fno.agents.cli import agents_app

        captured = {}

        def _fake_dispatch(**kw):
            captured.update(kw)
            return dispatch_mod.SpawnResult(
                kind="created", name=kw["name"], provider="claude",
                short_id="abcd1234",
                launch_account="makers", launch_account_source="config",
            )

        monkeypatch.setenv("FNO_SPAWN_GATE", "0")
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        # Pin the PYTHON lane: with the Rust runtime auto-route armed, the bg
        # branch execs the client before dispatch_spawn is ever reached (and
        # the probe proof: a real worker spawns).
        monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
        # The bg receipt flushes, then QoS-demotes a live short_id against the
        # roster; a stubbed back half has no worker to demote.
        monkeypatch.setattr(
            "fno.agents.spawn_gate.qos_demote_bg_worker", lambda *a, **k: None
        )
        monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", _fake_dispatch)
        result = CliRunner().invoke(
            agents_app,
            ["spawn", "--name", "b-prov", "hi", "--harness", "claude",
             "--substrate", "bg"],
        )
        assert result.exit_code == 0, result.output
        receipt = self._receipt_of(result.output)
        assert receipt["account"] == "makers"
        assert receipt["account_source"] == "config"


class TestVocabulary:
    def test_receipt_and_table_share_the_wire_values(self) -> None:
        assert spawn_flag_owners.CALLER == "caller"
        assert spawn_flag_owners.CONFIG == "config"
        assert spawn_flag_owners.ENV == "env"
        assert spawn_flag_owners.DEFAULT == "default"

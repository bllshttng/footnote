"""Launch-time headroom picking at the spawn seam (x-7d45, task 3.1).

No in-session credential swap is possible, so the only moment footnote can
choose an account is just before a process starts. These cover that seam:
the pick happens in ``dispatch_spawn`` (which every spawn path crosses, not
just the CLI), an explicit ``--account`` is never second-guessed, and a worker
that dies on quota poisons the next pick before its snapshot would refresh.

Run: cd cli && uv run pytest tests/unit/test_launch_pick.py -v
"""
from __future__ import annotations

import time
from pathlib import Path

import pytest
import tomli_w

from fno.agents import dispatch as dispatch_mod


def _write_config(tmp_path: Path, *, pick_on_launch: bool) -> None:
    alt, main = tmp_path / "claude-alt", tmp_path / "claude-main"
    for d in (alt, main):
        d.mkdir(exist_ok=True)
        (d / ".credentials.json").write_text("{}")
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "config.toml").write_text(
        tomli_w.dumps({
            "providers": {
                "active": "readyrule",
                "quota": {"pick_on_launch": pick_on_launch},
                "records": [
                    {"id": "readyrule", "name": "readyrule", "cli": "claude",
                     "auth": "managed", "config_dir": str(alt)},
                    {"id": "makers", "name": "makers", "cli": "claude",
                     "auth": "managed", "config_dir": str(main)},
                ],
            }
        }),
        encoding="utf-8",
    )


@pytest.fixture()
def armed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """readyrule EXHAUSTED, makers OK, picking armed."""
    _write_config(tmp_path, pick_on_launch=True)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
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


class TestPickAtLaunch:
    def test_picks_the_account_with_headroom(
        self, armed: Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """AC7-HP: the worker's env carries the healthy account's config dir."""
        env = dispatch_mod._pick_account_env()
        assert env is not None
        assert env["CLAUDE_CONFIG_DIR"] == str(armed / "claude-main")

    def test_receipt_names_the_picked_account_and_its_headroom(
        self, armed: Path, capsys: pytest.CaptureFixture[str]
    ) -> None:
        # A launch landing on a different account than expected is a billing
        # surprise; the receipt is the whole mitigation.
        dispatch_mod._pick_account_env()
        err = capsys.readouterr().err
        assert "account: makers (picked," in err
        assert "32%" in err

    def test_off_by_default(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        _write_config(tmp_path, pick_on_launch=False)
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        assert dispatch_mod._pick_account_env() is None

    def test_no_candidate_degrades_to_today_with_a_reason(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        """Fail-open, never fail-silent: the spawn proceeds and says why."""
        (tmp_path / ".fno").mkdir()
        (tmp_path / ".fno" / "config.toml").write_text(
            tomli_w.dumps({"providers": {
                "active": "solo",
                "quota": {"pick_on_launch": True},
                "records": [{"id": "solo", "name": "solo", "cli": "claude",
                             "auth": "managed"}],
            }}),
            encoding="utf-8",
        )
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / ".fno"))
        assert dispatch_mod._pick_account_env() is None
        assert "pick unavailable" in capsys.readouterr().err


class TestSpawnSeam:
    """AC8-CON: the picker is reached from every spawn path.

    A guard on one of N reachable paths is decorative. These call
    ``dispatch_spawn`` directly, bypassing the CLI's argument parsing entirely.
    """

    def _capture_create(self, monkeypatch: pytest.MonkeyPatch) -> dict:
        seen: dict = {}

        class _Created:
            short_id = "abc123"

        def _fake_create(**kwargs):
            seen.update(kwargs)
            return _Created()

        monkeypatch.setattr(dispatch_mod, "_claude_create_path", _fake_create)
        monkeypatch.setattr(dispatch_mod, "_emit_ev", lambda *a, **k: None)
        return seen

    def test_direct_call_applies_the_picked_overlay(
        self, armed: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        seen = self._capture_create(monkeypatch)
        dispatch_mod.dispatch_spawn(
            name="w1", message="hi", provider="claude", cwd=armed
        )
        assert seen["account_env"]["CLAUDE_CONFIG_DIR"] == str(armed / "claude-main")

    def test_explicit_account_is_never_overridden(
        self, armed: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """AC10-ERR: an explicit --account wins even when it reads EXHAUSTED."""
        seen = self._capture_create(monkeypatch)

        def _never(*a, **k):
            raise AssertionError("picker consulted despite an explicit account")

        monkeypatch.setattr(dispatch_mod, "_pick_account_env", _never)
        explicit = {"CLAUDE_CONFIG_DIR": str(armed / "claude-alt")}
        dispatch_mod.dispatch_spawn(
            name="w2", message="hi", provider="claude", cwd=armed,
            account_env=explicit,
        )
        assert seen["account_env"] == explicit


class TestQuotaDeath:
    def test_a_quota_death_cools_the_account_down(
        self, armed: Path
    ) -> None:
        # The snapshot can be probe_ttl_seconds stale, so without this the next
        # pick would hand the successor the account that just died.
        from fno.adapters.providers.runtime_state import HeadroomState, headroom

        env = {"CLAUDE_CONFIG_DIR": str(armed / "claude-main")}
        assert headroom("makers").state is not HeadroomState.EXHAUSTED
        dispatch_mod.note_quota_death(env, "Error: usage limit reached for this account")
        assert headroom("makers").state is HeadroomState.EXHAUSTED

    def test_an_unrelated_death_writes_nothing(self, armed: Path) -> None:
        from fno.adapters.providers.runtime_state import HeadroomState, headroom

        env = {"CLAUDE_CONFIG_DIR": str(armed / "claude-main")}
        dispatch_mod.note_quota_death(env, "Error: file not found")
        assert headroom("makers").state is not HeadroomState.EXHAUSTED

    def test_no_tail_is_a_no_op(self, armed: Path) -> None:
        dispatch_mod.note_quota_death({"CLAUDE_CONFIG_DIR": "/nope"}, None)

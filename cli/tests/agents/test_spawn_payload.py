from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from fno.paths_testing import use_tmpdir


BREVITY_MARKER = "<fno_relay_compression>"
FIXTURE = Path(__file__).resolve().parents[3] / "schemas" / "spawn-brevity.json"
ORIGINAL_PAYLOAD = """Report why `fno mail send` did not run.

```bash
fno mail send worker --raw '/fno:review'
```

Keep 80 words, not 81."""


def _setup_tmp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for key in ("FNO_AGENT_SELF", "FNO_AGENT_HARNESS", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(key, raising=False)


def test_python_background_spawn_appends_brevity_marker_after_exact_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    captured: dict[str, Any] = {}

    def fake_create(**kwargs: Any) -> DispatchAskResult:
        captured.update(kwargs)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)

    dispatch_spawn(
        name="brief-worker",
        message=ORIGINAL_PAYLOAD,
        provider="claude",
        cwd=tmp_path,
    )

    delivered = captured["message"]
    assert delivered.startswith(ORIGINAL_PAYLOAD + "\n\n")
    assert delivered.count(BREVITY_MARKER) == 1


def test_python_opencode_pane_submits_brevity_marker_after_exact_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import mux_spawn

    captured: dict[str, str] = {}

    def runner(argv: list[str], **_kwargs: Any) -> SimpleNamespace:
        if argv[1:4] == ["mux", "pane", "run"]:
            return SimpleNamespace(returncode=0, stdout="7\n", stderr="")
        if argv[1:4] == ["mux", "pane", "ls"]:
            return SimpleNamespace(returncode=0, stdout="[]", stderr="")
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(
        mux_spawn,
        "_await_interactive_readiness",
        lambda *_args, **_kwargs: ("ready", "ready-marker=idle_prompt"),
    )

    # **_kw absorbs seed_in_argv, which the real _submit_spawn_seed takes as a
    # keyword-only argument. A stub standing in for a growing signature must
    # not pin it.
    def capture_seed(
        _provider: str,
        _session: str,
        _pane_id: int,
        seed: str,
        _runner: Any,
        **_kw: Any,
    ) -> tuple[str, str, str]:
        captured["seed"] = seed
        return "submitted", "", "delivered"

    monkeypatch.setattr(mux_spawn, "_submit_spawn_seed", capture_seed)

    mux_spawn.dispatch_spawn_pane(
        name="brief-opencode",
        message=ORIGINAL_PAYLOAD,
        provider="opencode",
        cwd=tmp_path,
        runner=runner,
    )

    delivered = captured["seed"]
    assert delivered.startswith(ORIGINAL_PAYLOAD + "\n\n")
    assert delivered.count(BREVITY_MARKER) == 1


def test_python_runtime_constants_match_shared_cross_language_fixture() -> None:
    from fno.agents.spawn_payload import (
        BREVITY_END_MARKER,
        BREVITY_INSTRUCTION,
        BREVITY_MARKER as RUNTIME_MARKER,
    )

    fixture = json.loads(FIXTURE.read_text())
    assert fixture == {
        "marker": RUNTIME_MARKER,
        "end_marker": BREVITY_END_MARKER,
        "instruction": BREVITY_INSTRUCTION,
    }


def test_python_payload_enrichment_is_idempotent_and_preserves_empty_spawn() -> None:
    from fno.agents.spawn_payload import enrich_spawn_payload

    assert enrich_spawn_payload("") == ""
    enriched = enrich_spawn_payload(ORIGINAL_PAYLOAD)
    assert enrich_spawn_payload(enriched) == enriched
    assert enriched[: len(ORIGINAL_PAYLOAD)] == ORIGINAL_PAYLOAD


def test_python_payload_that_mentions_marker_still_gets_complete_guidance() -> None:
    from fno.agents.spawn_payload import BREVITY_END_MARKER, enrich_spawn_payload

    original = f"Explain the literal {BREVITY_MARKER} token."
    enriched = enrich_spawn_payload(original)
    assert enriched.startswith(original + "\n\n")
    assert enriched.count(BREVITY_MARKER) == 2
    assert enriched.endswith(BREVITY_END_MARKER)

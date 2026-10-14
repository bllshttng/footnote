"""`--permission-mode` passthrough across the spawn surfaces (x-dfa4).

Covers the per-provider mapping table (accepted value -> exact argv; unmappable
-> fail-closed error), the claude `--yolo` -> bypassPermissions fix, the
`--permission-mode`/`--yolo` mutual exclusion, and the config default that
applies to autonomous dispatchers only. No test performs a live spawn - the pane
dispatch is stubbed and the autonomous dispatchers' subprocess is mocked.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.agents.dispatch import DispatchAskError
from fno.agents.mux_spawn import build_pane_argv, permission_pane_tokens

CWD = Path("/tmp")


# --- mapping table (permission_pane_tokens) --------------------------------


@pytest.mark.parametrize(
    "provider,mode,expected",
    [
        ("claude", "acceptEdits", ["--permission-mode", "acceptEdits"]),
        ("claude", "plan", ["--permission-mode", "plan"]),
        ("gemini", "plan", ["--approval-mode", "plan"]),
        ("gemini", "yolo", ["--yolo"]),
        ("codex", "full-auto", ["--full-auto"]),
        ("codex", "yolo", ["--dangerously-bypass-approvals-and-sandbox"]),
        (
            "codex",
            "workspace-write:on-request",
            [
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request",
                "-c",
                "sandbox_workspace_write.network_access=true",
            ],
        ),
        (
            "codex",
            "workspace-write:never",
            [
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "never",
                "-c",
                "sandbox_workspace_write.network_access=true",
            ],
        ),
        ("opencode", "auto", ["--auto"]),
        ("agy", "skip", ["--dangerously-skip-permissions"]),
    ],
)
def test_mapping_accepts_provider_native_values(rust_door, provider, mode, expected):
    assert permission_pane_tokens(provider, mode) == expected


@pytest.mark.parametrize(
    "provider,mode",
    [
        ("opencode", "acceptEdits"),  # AC3-ERR: only 'auto' maps
        ("agy", "yolo-please"),  # not in agy's own vocabulary
        ("codex", "bogus"),  # not a shortcut or colon form
        ("codex", "workspace-write"),  # colon form needs both axes
        ("claude", ""),  # empty value required
    ],
)
def test_mapping_fail_closed_on_unmappable(rust_door, provider, mode):
    with pytest.raises(DispatchAskError) as exc:
        permission_pane_tokens(provider, mode)
    assert exc.value.exit_code == 2


@pytest.mark.dev_build
def test_codex_refusal_names_the_overlay_key(rust_door):
    """fno's own builtin carries as codex's bypass flag; a genuinely foreign
    word still refuses and names the config key that fixes it (AC5-HP)."""
    assert permission_pane_tokens("codex", "bypassPermissions") == [
        "--dangerously-bypass-approvals-and-sandbox"
    ]
    with pytest.raises(DispatchAskError) as exc:
        permission_pane_tokens("codex", "acceptEdits")
    assert "agents.*.harness.codex.permission_mode" in str(exc.value)


@pytest.mark.dev_build
def test_codex_pane_bounded_default_rides_the_rust_owner(rust_door, monkeypatch, tmp_path):
    """AC3-HP: with no mode and no yolo, the pane's bounded default IS the
    thread lane's workspace-write:never pair, tokens straight from
    permission_pane_tokens - pane and thread start from one posture, and the
    network override rides with it."""
    monkeypatch.setattr("fno.agents.mux_spawn.worker_writable_dirs", lambda *a, **k: [])
    argv = build_pane_argv("codex", "hi", tmp_path, False, None, None)
    assert argv[:5] == ["codex", "--remote", "unix://", "-C", str(tmp_path)]
    i = argv.index("--sandbox")
    assert argv[i : i + 6] == [
        "--sandbox",
        "workspace-write",
        "--ask-for-approval",
        "never",
        "-c",
        "sandbox_workspace_write.network_access=true",
    ]


# --- agy pane posture: default bypass, explicit mode replaces it -------------


@pytest.fixture
def rust_door(monkeypatch):
    """Pin the posture door to THIS checkout's fno-agents build; skip where
    the checkout has none (the same contract conftest.native_backlog_door
    implements). A stale installed binary must not answer for this tree."""
    from fno.rust_binary import find_dev_binary

    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    monkeypatch.setenv("FNO_AGENTS_BIN", str(binary))


@pytest.mark.dev_build
def test_agy_pane_default_keeps_bypass(rust_door, capsys):
    argv = build_pane_argv("agy", "hi", CWD, False, "uuid", None, None)
    assert "--dangerously-skip-permissions" in argv
    # The launch names the posture it ran with, once, on stderr.
    captured = capsys.readouterr().err
    assert "agy posture: bypass (lane-default)" in captured


@pytest.mark.dev_build
def test_agy_pane_mode_replaces_bypass(rust_door):
    argv = build_pane_argv("agy", "hi", CWD, False, "uuid", None, "plan")
    assert "--mode" in argv and "plan" in argv
    assert "--dangerously-skip-permissions" not in argv


@pytest.mark.dev_build
def test_agy_pane_yolo_plus_mode_refuses(rust_door):
    # AC13-ERR flavor: one knob at a time, before any spawn.
    with pytest.raises(DispatchAskError):
        build_pane_argv("agy", "hi", CWD, True, "uuid", None, "plan")


# --- build_pane_argv integration -------------------------------------------


def test_codex_pane_colon_two_axis_form():
    """AC2-HP: codex <sandbox>:<approval> -> --sandbox X --ask-for-approval Y."""
    argv = build_pane_argv(
        "codex", "hi", CWD, False, None, None, "workspace-write:on-request"
    )
    assert argv[:8] == [
        "codex",
        "--remote",
        "unix://",
        "-C",
        str(CWD),
        "--sandbox",
        "workspace-write",
        "--ask-for-approval",
    ]


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini", "opencode", "agy"])
def test_unset_is_byte_identical(provider):
    """AC7-EDGE: no mode + no yolo -> byte-identical to pre-feature argv."""
    with_arg = build_pane_argv(provider, "hi", CWD, False, "uuid", None, None)
    without = build_pane_argv(provider, "hi", CWD, False, "uuid", None)
    assert with_arg == without


# --- cmd_spawn CLI wiring ---------------------------------------------------


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_harness_markers(monkeypatch):
    for m in (
        "CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"
    ):
        monkeypatch.delenv(m, raising=False)


def _stub_pane_path(monkeypatch) -> dict:
    received: dict = {}
    from fno.agents import mux_spawn, spawn_gate

    class _Gate:
        def release(self) -> None:
            pass

    def fake_dispatch_spawn_pane(**kwargs):
        received.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="sess-1",
            pane_id=1,
            child_pid=None,
            session_uuid=None,
        )

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *a, **k: None)
    monkeypatch.setattr(
        mux_spawn, "dispatch_spawn_bounded_pane", fake_dispatch_spawn_pane
    )
    return received


# --- AC8: config default applies to autonomous dispatchers only -------------


def _fake_settings(mode: str):
    return SimpleNamespace(
        agents=SimpleNamespace(defaults=SimpleNamespace(permission_mode=mode))
    )


def _capture_spawn(monkeypatch, module):
    """Mock the spawn subprocess at ``module.subprocess.run``; return the cmd."""
    captured: dict = {}
    real_run = module.subprocess.run

    def fake_run(cmd, **kwargs):
        # The mint is a real pre-spawn subprocess (x-84b2): serve it with the
        # real binary and keep it out of the capture, which pins the SPAWN argv.
        if {"name-mint", "name-codes", "name-parse"} & {str(p) for p in cmd}:
            return real_run(cmd, **kwargs)
        # Native event commits are infrastructure: ride the real binary too.
        if {"doctor", "event"} <= {str(p) for p in cmd}:
            return real_run(cmd, **kwargs)
        captured["cmd"] = cmd
        return SimpleNamespace(
            returncode=0, stdout='{"name":"w","short_id":"abcd1234"}', stderr=""
        )

    monkeypatch.setattr(module.subprocess, "run", fake_run)
    return captured


def test_think_worker_reads_config_default(monkeypatch):
    from fno.provenance import spawn_think

    captured = _capture_spawn(monkeypatch, spawn_think)
    monkeypatch.setattr(
        spawn_think, "_settings_for", lambda root: _fake_settings("plan")
    )
    spawn_think._spawn_think_worker("x-test", "prompt", None, "slug")
    assert "--permission-mode" in captured["cmd"]
    i = captured["cmd"].index("--permission-mode")
    assert captured["cmd"][i + 1] == "plan"


def test_think_worker_codex_uses_shared_dispatch_default(monkeypatch):
    from fno.provenance import spawn_think

    captured = _capture_spawn(monkeypatch, spawn_think)
    settings = SimpleNamespace(
        agents=SimpleNamespace(defaults=SimpleNamespace(permission_mode="")),
        dispatch=SimpleNamespace(
            harness="",
            substrate="",
            command="",
            auto_merge=False,
            allowed_verbs=["/target", "/think"],
        ),
    )
    monkeypatch.setattr(spawn_think, "_settings_for", lambda root: settings)

    spawn_think._spawn_think_worker(
        "x-test",
        "/think x-test\n\nfull context",
        None,
        "slug",
        provider="codex",
    )

    cmd = captured["cmd"]
    assert cmd[cmd.index("--harness") + 1] == "codex"
    assert cmd[cmd.index("--substrate") + 1] == "thread"
    assert cmd[-1] == "$fno:think x-test\n\nfull context"


def test_think_worker_scopes_tier_to_resolved_dispatch_harness(monkeypatch):
    from fno.provenance import spawn_think

    captured = _capture_spawn(monkeypatch, spawn_think)
    settings = SimpleNamespace(
        agents=SimpleNamespace(defaults=SimpleNamespace(permission_mode="")),
        dispatch=SimpleNamespace(
            harness="codex",
            substrate="",
            command="",
            auto_merge=False,
            allowed_verbs=["/target", "/think"],
        ),
    )
    monkeypatch.setattr(spawn_think, "_settings_for", lambda root: settings)
    model_scopes: list[str] = []
    monkeypatch.setattr(
        spawn_think._route_resolve,
        "node_model",
        lambda node, provider, **_kw: model_scopes.append(provider) or "codex-tier-model",
    )

    spawn_think._spawn_think_worker(
        "x-test",
        "/think x-test\n\nfull context",
        None,
        "slug",
        model="",
        node={"difficulty": "high"},
    )

    cmd = captured["cmd"]
    assert model_scopes == ["codex"]
    assert cmd[cmd.index("--model") + 1] == "codex-tier-model"


def test_think_worker_explicit_pane_uses_shared_capability_gate(monkeypatch):
    from fno.provenance import spawn_think

    captured = _capture_spawn(monkeypatch, spawn_think)
    settings = SimpleNamespace(
        agents=SimpleNamespace(defaults=SimpleNamespace(permission_mode="plan")),
        dispatch=SimpleNamespace(
            harness="",
            substrate="pane",
            command="",
            auto_merge=False,
            allowed_verbs=["/target", "/think"],
        ),
    )
    monkeypatch.setattr(spawn_think, "_settings_for", lambda root: settings)

    spawn_think._spawn_think_worker(
        "x-test",
        "/think x-test\n\nfull context",
        None,
        "slug",
        provider="codex",
    )

    cmd = captured["cmd"]
    assert cmd[cmd.index("--harness") + 1] == "codex"
    assert cmd[cmd.index("--substrate") + 1] == "pane"
    assert cmd[cmd.index("--permission-mode") + 1] == "plan"
    assert cmd[-1] == "$fno:think x-test\n\nfull context"


# --- bash surface: /agent spawn (spawn.sh) ----------------------------------

import shutil  # noqa: E402
import stat  # noqa: E402
import subprocess as _sp  # noqa: E402


def _node_row(
    node_id: str,
    difficulty: str | None = "low",
    verb: str | None = None,
    model: str | None = "glm-5.3-flash[1m]",
) -> dict:
    """The minimal node dict tests pass to the dispatcher.

    Key presence is what the projection check reads; difficulty low derives
    /target, matching what the builtin path asserted before the None branch
    was deleted. An out-of-family ``verb`` rides the row so the lifecycle
    table abstains and the explicit verb wins, as the deleted None path did.
    The default model pin clears the seam gate (x-8fb2): CI runs with no
    routing config, so the grid declines and an unpinned spawn would refuse."""
    return {
        "id": node_id,
        "dispatch_verb": verb or "",
        "difficulty": difficulty,
        "model": model,
    }


# --- x-6863: one vocabulary, in Rust; the Python holds no second table ------


def _rust_answer(provider: str, mode: str, substrate: str | None) -> dict:
    from fno.rust_binary import verb_call

    payload = {"provider": provider, "mode": mode}
    if substrate:
        payload["substrate"] = substrate
    return verb_call("permission-tokens", payload, Exception)


@pytest.mark.parametrize(
    "provider,mode",
    [
        ("claude", "bypassPermissions"),
        ("codex", "full-auto"),
        ("codex", "workspace-write:on-request"),
        ("gemini", "yolo"),
        ("opencode", "auto"),
        ("agy", "skip"),
        ("cursor-agent", "force"),
        ("grok", "dontAsk"),
    ],
)
def test_python_bridge_matches_the_rust_vocabulary(provider, mode):
    """AC4-EDGE: the bridge and the owner answer identically, so the tree
    holds ONE table. The refusal messages compare byte-for-byte too."""

    py_refusal = None
    py_tokens = None
    try:
        py_tokens = permission_pane_tokens(provider, mode)
    except DispatchAskError as exc:
        py_refusal = str(exc)
    rust = _rust_answer(provider, mode, None)
    if rust.get("refusal"):
        assert py_refusal is not None, f"python accepted what rust refused: {rust}"
        assert py_refusal == rust["refusal"]
        with pytest.raises(DispatchAskError):
            permission_pane_tokens(provider, mode)
    else:
        assert py_refusal is None
        assert py_tokens == [str(t) for t in rust["tokens"]]

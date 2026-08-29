"""CLI wiring for `fno agents spawn --account <id>` (x-d012, US2).

The four-lane overlay resolution is unit-tested in
`src/fno/agents/test_account_env.py`; this pins the CLI wiring: a resolved
overlay reaches dispatch_spawn_pane as `account_env` and the receipt names the
account (AC1-HP), and a resolver refusal fails closed before any spawn
(AC1-ERR / AC2-ERR).
"""
from __future__ import annotations

import json
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_harness_markers(monkeypatch):
    for m in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)


def _stub_pane_path(monkeypatch) -> dict:
    received: dict = {}
    from fno.agents import mux_spawn, spawn_gate

    class _Gate:
        def release(self) -> None:
            pass

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *a, **k: None)

    def fake_pane(**kwargs):
        received.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"], provider=kwargs["provider"], session="s",
            pane_id=1, child_pid=None, session_uuid=None,
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_pane)
    return received


def test_account_overlay_threads_to_pane_and_receipt(monkeypatch, runner):
    """AC1-HP: a resolved overlay reaches dispatch_spawn_pane; receipt names it."""
    received = _stub_pane_path(monkeypatch)
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    # cmd_spawn calls resolve_account_overlay_or_exit, which wraps
    # resolve_account_overlay; patching the inner call is enough.
    monkeypatch.setattr(
        ae, "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )

    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w1", "hi", "--account", "readyrule", "--here"]
    )
    assert result.exit_code == 0, result.output
    assert received["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude"}
    receipt = json.loads(
        next(ln for ln in result.output.splitlines() if ln.startswith("{"))
    )
    assert receipt["account"] == "readyrule"


def test_dispatch_account_threads_to_pane_and_receipt(monkeypatch, runner):
    """Capability handoff can prove the destination provider record from receipt."""
    received = _stub_pane_path(monkeypatch)
    from fno.adapters.providers import dispatch, loader

    monkeypatch.setattr(
        loader,
        "load_providers",
        lambda **_kwargs: SimpleNamespace(
            by_id={"makers": SimpleNamespace(harness="claude")}
        ),
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_env",
        lambda *_args, **_kwargs: {"CLAUDE_CONFIG_DIR": "/x/.claude-makers"},
    )

    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        [
            "spawn", "--name", "w1", "hi", "--harness", "claude",
            "--dispatch-account", "makers", "--here",
        ],
    )
    assert result.exit_code == 0, result.output
    assert received["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude-makers"}
    receipt = json.loads(
        next(ln for ln in result.output.splitlines() if ln.startswith("{"))
    )
    assert receipt["dispatch_account"] == "makers"


def test_account_refusal_fails_closed(monkeypatch, runner):
    """AC1-ERR: a resolver refusal exits 2 before any spawn (pane stub untouched)."""
    received = _stub_pane_path(monkeypatch)
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountResolutionError

    def boom(*a, **k):
        raise AccountResolutionError("account 'nope' is not registered.")

    monkeypatch.setattr(ae, "resolve_account_overlay", boom)

    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "--name", "w1", "hi", "--account", "nope"])
    assert result.exit_code == 2
    assert "is not registered" in result.output
    assert received == {}  # never reached the pane dispatch


def test_account_non_claude_provider_refused(monkeypatch, runner):
    """--account with a non-claude provider is a user error caught before spawn."""
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w1", "hi", "--harness", "codex", "--account", "x"]
    )
    assert result.exit_code == 2
    assert "claude-only" in result.output
    assert received == {}


def test_account_plus_route_composes(monkeypatch, runner):
    """x-5ed4: --account + --route compose (account profile + vendor route).
    Both overlays reach dispatch_spawn; the old 'cannot combine' exit 2 is gone."""
    from fno.agents import dispatch, spawn_gate
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: type("G", (), {"release": lambda self: None})())
    monkeypatch.setattr(
        ae,
        "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )
    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return dispatch.SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id="abcd1234"
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--account", "readyrule", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    assert "cannot combine" not in result.output
    # The account profile AND the vendor route both reached dispatch.
    assert captured["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude"}
    assert captured["route_env"]["ANTHROPIC_AUTH_TOKEN"] == "zk-live"
    assert captured["route_env"]["ANTHROPIC_MODEL"] == "glm-5.2"


def test_account_plus_role_composes(monkeypatch, runner):
    """x-5ed4: --account + --role compose too (the role's fail-safe route + the
    account profile); no longer refused."""
    from fno.agents import dispatch, spawn_gate
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: type("G", (), {"release": lambda self: None})())
    monkeypatch.setattr(
        ae,
        "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )
    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return dispatch.SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id="abcd1234"
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--account", "readyrule", "--role", "tidy"],
    )
    assert result.exit_code == 0, result.output
    assert "cannot combine" not in result.output
    assert captured["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude"}
    # role tidy auto-routes to zai (DEFAULT_ROUTED_ROLES) with a key present.
    assert captured["route_env"]["ANTHROPIC_AUTH_TOKEN"] == "zk-live"


def test_account_plus_route_composes_under_inherited_managed(monkeypatch, runner):
    """x-3db9 P1: under an inherited FNO_PROVIDER_AUTH=managed (spawning from
    inside a managed worker), --account + --route still composes. The managed-
    OAuth half-composition refusal is skipped when an account overlay pins a
    profile: the route is fail-closed (its own token) and route-wins is atomic,
    so the split-brain the guard existed to prevent cannot recur."""
    from fno.agents import dispatch, spawn_gate
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: type("G", (), {"release": lambda self: None})())
    monkeypatch.setattr(
        ae,
        "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )
    captured: dict = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return dispatch.SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id="abcd1234"
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--account", "readyrule", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    assert captured["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude"}
    assert captured["route_env"]["ANTHROPIC_AUTH_TOKEN"] == "zk-live"


def test_composed_receipt_names_live_credential_and_payer(monkeypatch, runner):
    """AC2-HP (x-8552): a composed spawn's receipt carries auth naming the
    route's vendor and bills naming that vendor, beside account naming the
    profile - all derived from the composed env, never the flags. The cure for
    `--account makers -P zai` reading as "billing makers"."""
    from fno.agents import dispatch, spawn_gate
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: type("G", (), {"release": lambda self: None})())
    monkeypatch.setattr(
        ae,
        "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )

    def fake_dispatch(**kwargs):
        return dispatch.SpawnResult(
            kind="created",
            name=kwargs["name"],
            provider=kwargs["harness"],
            short_id="abcd1234",
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--account", "readyrule", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(
        next(ln for ln in result.output.splitlines() if '"short_id"' in ln)
    )
    assert receipt["account"] == "readyrule"
    assert receipt["auth"] == "route:zai"
    assert receipt["bills"] == "zai"
    # The same content reaches the interactive surface, picker-shaped.
    assert "profile only" in result.output
    assert "bills zai" in result.output


def test_account_only_receipt_has_no_credential_fields(monkeypatch, runner):
    """AC3-EDGE (x-8552): an --account spawn with no route emits no auth/bills -
    its receipt stays byte-identical to the pre-x-8552 shape."""
    received = _stub_pane_path(monkeypatch)
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    monkeypatch.setattr(
        ae, "resolve_account_overlay",
        lambda *a, **k: AccountOverlay(
            "readyrule", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"
        ),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w1", "hi", "--account", "readyrule", "--here"]
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(
        next(ln for ln in result.output.splitlines() if ln.startswith("{"))
    )
    assert receipt["account"] == "readyrule"
    assert "auth" not in receipt
    assert "bills" not in receipt


# ---------------------------------------------------------------------------
# x-c33e: the codex thread client seals footnote's roots around a HOME overlay
# ---------------------------------------------------------------------------


def test_codex_thread_client_env_seals_our_state_roots(monkeypatch, tmp_path):
    """The lane a fixed cutover lands on forwards HOME without moving our roots.

    Unlike the spawn front door there is no argv carrier here: the env is the
    only channel to the app-server child. So the credential HOME stays and
    footnote's own roots are pinned around it, or the client's registry read and
    the child's claim would resolve under the account's home (x-c33e).
    """
    import json

    from fno import rust_binary
    from fno.agents import dispatch as dsp

    monkeypatch.setenv("HOME", "/real/home")
    monkeypatch.delenv("FNO_AGENTS_HOME", raising=False)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: tmp_path / "fno-agents")

    captured = {}

    def fake_run(argv, **kw):
        captured["env"] = kw.get("env")
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps({"session_id": "0198c0de-1111-7000-8000-00000000000a"}),
            stderr="",
        )

    monkeypatch.setattr(dsp.subprocess, "run", fake_run)

    dsp._codex_thread_spawn(
        name="t-cutover",
        message="/target ab-2222aaaa",
        cwd=tmp_path,
        from_name="",
        model=None,
        yolo=False,
        account_env={"HOME": "/accounts/zai-1/home"},
        route_env=None,
    )

    env = captured["env"]
    # The credential still reaches the launched harness.
    assert env["HOME"] == "/accounts/zai-1/home"
    # The positive markers: our roots name the REAL home, not the account's.
    assert env["FNO_AGENTS_HOME"] == "/real/home/.fno/agents"
    assert env["FNO_CLAIMS_ROOT"] == "/real/home"


def test_codex_thread_client_env_unsealed_without_a_home_overlay(
    monkeypatch, tmp_path
):
    """A route-only overlay moves no root, so nothing is pinned."""
    import json

    from fno import rust_binary
    from fno.agents import dispatch as dsp

    monkeypatch.setenv("HOME", "/real/home")
    monkeypatch.delenv("FNO_AGENTS_HOME", raising=False)
    monkeypatch.setattr(rust_binary, "resolve_binary", lambda: tmp_path / "fno-agents")

    captured = {}

    def fake_run(argv, **kw):
        captured["env"] = kw.get("env")
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps({"session_id": "0198c0de-1111-7000-8000-00000000000a"}),
            stderr="",
        )

    monkeypatch.setattr(dsp.subprocess, "run", fake_run)

    dsp._codex_thread_spawn(
        name="t-routed",
        message="/target ab-2222aaaa",
        cwd=tmp_path,
        from_name="",
        model=None,
        yolo=False,
        account_env=None,
        route_env={"ANTHROPIC_BASE_URL": "https://x"},
    )

    env = captured["env"]
    assert env["HOME"] == "/real/home"
    assert "FNO_AGENTS_HOME" not in env


def test_codex_one_shot_refuses_a_pinned_account(monkeypatch, tmp_path):
    """A lane that cannot carry the overlay refuses instead of launching.

    _codex_create_path takes no account_env, and cmd_spawn exports only
    PROVENANCE_KEYS to os.environ for one-shots, so a resolved overlay would
    reach nothing here. Launching anyway would bill the ambient account
    silently, which is the failure both account flags are fail-closed against.
    """
    from fno.agents import dispatch as dsp

    def boom(**kw):
        pytest.fail("must not create a codex agent on the ambient account")

    monkeypatch.setattr(dsp, "_codex_create_path", boom)

    with pytest.raises(dsp.DispatchAskError) as exc:
        dsp.dispatch_spawn(
            name="t-once",
            message="hi",
            provider="codex",
            cwd=tmp_path,
            once=True,
            account_env={"HOME": "/accounts/zai-1/home"},
        )
    assert "one-shot" in str(exc.value)
    assert "--substrate thread" in str(exc.value)

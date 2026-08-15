"""x-ae2d: a routed worker's route survives a relaunch, or the relaunch refuses.

Two halves, matching the node's two mechanisms. The spawn seams record the PATH
of the route-settings file a worker was launched with (never its contents, which
include a live ``ANTHROPIC_AUTH_TOKEN``), and every relaunch entry point either
re-applies that route or refuses non-zero naming it.

The distinction that keeps this honest: ``claude attach`` opens a session that is
still running, so neither ``fno agents attach`` nor ``fno agents resume`` (which
IS ``claude attach``) can lose a route. The one claude door that starts a new
supervisor is the ``--resume`` revive spawn, so that is where the guard lives.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir

ROUTE_ENV = {
    "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "zai-secret-token",
    "ANTHROPIC_MODEL": "glm-5.2",
}


class _FakeRunner:
    """Mux pane runner: a pane spawn returns pane 7 with child pid 4242."""

    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        if argv[1:4] == ["mux", "pane", "run"]:
            return subprocess.CompletedProcess(argv, 0, "7\n", "")
        if argv[1:4] == ["mux", "pane", "ls"]:
            out = json.dumps(
                [{"pane_id": 7, "squad_id": 1, "tab_id": 1, "cwd": "/w", "child_pid": 4242}]
            )
            return subprocess.CompletedProcess(argv, 0, out, "")
        # A codex pane spawn probes liveness and retains the pane's output while
        # it waits for a session binding. 11 = the pane is up.
        if argv[1:4] == ["mux", "pane", "wait"]:
            return subprocess.CompletedProcess(argv, 11, "", "")
        if argv[1:4] == ["mux", "pane", "read"]:
            return subprocess.CompletedProcess(argv, 0, "", "")
        raise AssertionError(f"unexpected invocation: {argv}")


def _spawn_pane(monkeypatch, tmp_path, provider="claude", **kwargs):
    use_tmpdir(monkeypatch, tmp_path)
    for var in ("FNO_SESSION", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(var, raising=False)
    from fno.agents.mux_spawn import dispatch_spawn_pane

    return dispatch_spawn_pane(
        name="router",
        message="go",
        provider=provider,
        cwd=tmp_path,
        runner=_FakeRunner(),
        **kwargs,
    )


# --- 1.2: the spawn seam records the route path ------------------------------


def test_routed_pane_spawn_records_an_existing_route_settings_path(tmp_path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    _spawn_pane(monkeypatch, tmp_path, route_env=dict(ROUTE_ENV))
    row = load_registry()[0]
    assert row.route_settings_path, "a routed pane must record the file it launched with"
    assert Path(row.route_settings_path).exists()


def test_unrouted_spawn_records_no_route_path(tmp_path, monkeypatch) -> None:
    from fno.agents.registry import load_registry

    _spawn_pane(monkeypatch, tmp_path)
    assert load_registry()[0].route_settings_path is None


def test_ac7_registry_stores_the_path_never_the_route_contents(tmp_path, monkeypatch) -> None:
    """AC7: the registry file carries a path and no credential."""
    from fno import paths

    _spawn_pane(monkeypatch, tmp_path, route_env=dict(ROUTE_ENV))
    raw = paths.agents_registry_path().read_text(encoding="utf-8")
    assert "route-settings" in raw  # the path IS recorded
    assert "ANTHROPIC_AUTH_TOKEN" not in raw
    assert "zai-secret-token" not in raw
    assert "ANTHROPIC_BASE_URL" not in raw


def test_ac6_con_legacy_row_without_the_field_loads_as_never_routed(
    tmp_path, monkeypatch
) -> None:
    """AC6-CON: a row written before v12 loads, and reads as never-routed."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    from fno.agents.registry import load_registry

    target = paths.agents_registry_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(
        json.dumps(
            {
                "schema_version": 11,
                "agents": [
                    {
                        "name": "old",
                        "cwd": "/w",
                        "log_path": "",
                        "harness": "claude",
                        "short_id": "deadbeef",
                        "created_at": "2026-08-01T00:00:00Z",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    row = load_registry()[0]
    assert row.name == "old"
    assert row.route_settings_path is None


def test_route_path_round_trips_through_the_registry(tmp_path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    write_registry(
        [
            AgentEntry(
                name="router",
                cwd="/w",
                log_path="",
                harness="claude",
                harness_session_id="sess-router",  # x-7bcd: needs a resolvable handle
                short_id="deadbeef",
                route_settings_path="/tmp/route-settings/abc.json",
            )
        ]
    )
    assert load_registry()[0].route_settings_path == "/tmp/route-settings/abc.json"


def test_route_settings_path_for_is_stable_and_route_wins(tmp_path, monkeypatch) -> None:
    """One precedence rule: route beats account, and re-asking is idempotent."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.model_routing import (
        materialize_route_settings,
        route_settings_path_for,
    )

    assert route_settings_path_for(None, None) is None
    routed = route_settings_path_for(dict(ROUTE_ENV))
    assert routed == materialize_route_settings(dict(ROUTE_ENV))
    # Composed: the route wins the settings file (x-5ed4), so the recorded path
    # is the route's, not the account's.
    assert route_settings_path_for(dict(ROUTE_ENV), {"CLAUDE_CONFIG_DIR": "/cfg"}) == routed


# --- 1.3: the revive door restores the route, or refuses ---------------------


def _routed_claude_row(tmp_path, monkeypatch):
    """A registry holding one routed claude worker, plus its route file path."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.model_routing import materialize_route_settings
    from fno.agents.registry import AgentEntry, write_registry

    path = materialize_route_settings(dict(ROUTE_ENV))
    write_registry(
        [
            AgentEntry(
                name="router",
                cwd=str(tmp_path),
                log_path="",
                harness="claude",
                short_id="deadbeef",
                harness_session_id="sess-1",
                route_settings_path=path,
            )
        ]
    )
    return path


def test_ac2_hp_revive_restores_the_recorded_route(tmp_path, monkeypatch) -> None:
    """AC2-HP: a relaunch comes back on the route it was launched with."""
    from fno.agents.dispatch import restore_route_for_relaunch
    from fno.agents.registry import load_registry

    _routed_claude_row(tmp_path, monkeypatch)
    got = restore_route_for_relaunch(load_registry()[0])
    # Exactly the route's own keys. The stored file also carries the auth-scrub
    # floor as empty strings, which means "unset" only to claude reading a
    # settings FILE - replaying it as process env would hand the revived worker
    # an ANTHROPIC_API_KEY="" the original launch never had.
    assert got == ROUTE_ENV


def test_ac3_err_revive_refuses_when_the_route_file_is_gone(tmp_path, monkeypatch) -> None:
    """AC3-ERR: refuse non-zero, name the route, start nothing."""
    from fno.agents.dispatch import DispatchAskError, restore_route_for_relaunch
    from fno.agents.registry import load_registry

    path = _routed_claude_row(tmp_path, monkeypatch)
    Path(path).unlink()
    with pytest.raises(DispatchAskError) as exc:
        restore_route_for_relaunch(load_registry()[0])
    assert path in str(exc.value)
    assert exc.value.exit_code == 2


def test_ac3_err_revive_refuses_on_a_malformed_route_file(tmp_path, monkeypatch) -> None:
    from fno.agents.dispatch import DispatchAskError, restore_route_for_relaunch
    from fno.agents.registry import load_registry

    path = _routed_claude_row(tmp_path, monkeypatch)
    Path(path).write_text("{not json", encoding="utf-8")
    with pytest.raises(DispatchAskError) as exc:
        restore_route_for_relaunch(load_registry()[0])
    assert exc.value.exit_code == 2


def test_ac5_hp_a_never_routed_row_restores_nothing(tmp_path, monkeypatch) -> None:
    """AC5-HP: a worker that was never routed sees no new behavior."""
    from fno.agents.dispatch import restore_route_for_relaunch
    from fno.agents.registry import AgentEntry

    row = AgentEntry(name="plain", cwd="/w", log_path="", harness="claude")
    assert restore_route_for_relaunch(row) is None


def test_ac4_err_a_codex_pane_records_no_route_so_it_cannot_half_restore(
    tmp_path, monkeypatch
) -> None:
    """AC4-ERR, the honest half: no door may relaunch on HALF a route.

    A codex route lives in `-c` config args, not the env, so recording
    `CodexRoute.env` would let a relaunch land on codex's default provider while
    holding the route's API key - working, wrong, and silent. Not recording is
    what makes that impossible; codex relaunch stays exactly as it is today.
    """
    from fno.agents.registry import load_registry

    _spawn_pane(monkeypatch, tmp_path, provider="codex", route_env=dict(ROUTE_ENV))
    assert load_registry()[0].route_settings_path is None


def test_restore_never_replays_the_auth_scrub_floor(tmp_path, monkeypatch) -> None:
    """The scrub floor is a settings-file convention, not process env.

    `materialize_route_settings` writes every SCRUB_AUTH_VARS entry as "" and
    the route on top. claude reads an empty settings value as unset; a process
    environment has no such rule, so replaying the floor would give the revived
    worker a present-but-blank credential the original launch never carried.
    """
    from fno.agents.account_env import SCRUB_AUTH_VARS
    from fno.agents.dispatch import restore_route_for_relaunch
    from fno.agents.registry import load_registry

    _routed_claude_row(tmp_path, monkeypatch)
    got = restore_route_for_relaunch(load_registry()[0])
    assert all(v != "" for v in got.values())
    blank = [v for v in SCRUB_AUTH_VARS if v not in ROUTE_ENV]
    assert blank, "fixture assumes the floor is wider than the route"
    assert not (set(blank) & set(got)), "an unset scrub var must not reach the spawn env"


def test_an_explicit_account_composes_with_a_restored_route(
    tmp_path, monkeypatch
) -> None:
    """--account and a route compose; the revive path must not refuse the pair.

    Nothing in `fno agents spawn` refuses --account alongside --route: the route
    wins endpoint+auth+model through the settings file while the account's
    CLAUDE_CONFIG_DIR rides the spawn env to pick the per-account daemon
    (x-5ed4). A restored route is a route like any other, so the revive path
    inherits that contract rather than inventing a refusal for it.
    """
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    _routed_claude_row(tmp_path, monkeypatch)
    seen: dict = {}

    def _fake_bg_create(**kwargs):
        seen.update(kwargs)
        raise DispatchAskError("stop here", exit_code=99)

    from fno.agents.harnesses import claude as claude_mod

    monkeypatch.setattr(claude_mod, "bg_create", _fake_bg_create)
    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="router",
            message="go",
            provider="claude",
            cwd=tmp_path,
            resume_session_id="sess-1",
            account_env={"CLAUDE_CONFIG_DIR": "/cfg"},
        )
    assert exc.value.exit_code == 99, "the pair must reach the launch, not be refused"
    assert seen["account_env"] == {"CLAUDE_CONFIG_DIR": "/cfg"}, "account survives"
    assert seen["route_env"] == ROUTE_ENV, "and the restored route rides with it"


def test_a_role_that_resolves_to_nothing_still_restores_the_route(
    tmp_path, monkeypatch
) -> None:
    """A NAMED role is not a resolved route.

    `resolve_route` is fail-safe: a protected role, a disabled block, an
    unconfigured provider, or a missing key all return None. Keying the restore
    on "was --role mentioned" would skip it in exactly those cases and relaunch
    on the default account - the silent fallback this whole node exists to stop.
    """
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    path = _routed_claude_row(tmp_path, monkeypatch)
    Path(path).unlink()  # make the restore refuse, so it is observable
    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="router",
            message="go",
            provider="claude",
            cwd=tmp_path,
            resume_session_id="sess-1",
            role="a-role-that-resolves-to-nothing",
        )
    assert path in str(exc.value), "the restore must run even when a role was named"


def test_a_route_file_holding_only_the_scrub_floor_refuses(tmp_path, monkeypatch) -> None:
    """Readable but routeless is the same silent fallback as missing."""
    from fno.agents.dispatch import DispatchAskError, restore_route_for_relaunch
    from fno.agents.account_env import SCRUB_AUTH_VARS
    from fno.agents.registry import load_registry

    path = _routed_claude_row(tmp_path, monkeypatch)
    Path(path).write_text(
        json.dumps({"env": {v: "" for v in SCRUB_AUTH_VARS}}), encoding="utf-8"
    )
    with pytest.raises(DispatchAskError) as exc:
        restore_route_for_relaunch(load_registry()[0])
    assert exc.value.exit_code == 2


def test_a_renamed_relaunch_of_the_same_transcript_still_restores(
    tmp_path, monkeypatch
) -> None:
    """The source row is the transcript's, not this spawn's name.

    `spawn other-name --resume <uuid>` relaunches the same transcript under a
    fresh row. Keying the restore on a same-name revive would leave every
    renamed relaunch silently unrouted - a guard on one of the two ways in.
    """
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    path = _routed_claude_row(tmp_path, monkeypatch)
    Path(path).unlink()  # make the restore refuse, so it is observable
    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="a-different-name",
            message="go",
            provider="claude",
            cwd=tmp_path,
            resume_session_id="sess-1",
        )
    assert path in str(exc.value)
    assert "router" in str(exc.value), "the refusal names the row that owns the route"


def test_the_account_picker_never_fires_on_a_revive(tmp_path, monkeypatch, capsys) -> None:
    """An auto-picked account must not turn a revive into an exit-2 refusal.

    `pick_on_launch` fills in `--account` for a spawn that named none, on BOTH
    seams: the argv seam (`_pick_account_at_seam`) and the in-process one here.
    Left running on a revive it prints an `account: <id> (picked)` receipt for a
    destination the operator never chose, and composes that overlay with the
    restored route - two axes merged by an advisory guess. A revive continues an
    existing transcript, and that transcript lives under the config dir it was
    created in, so a picked CLAUDE_CONFIG_DIR resumes into a directory where the
    uuid does not exist.
    """
    import fno.agents.rust_runtime as rr
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    path = _routed_claude_row(tmp_path, monkeypatch)
    monkeypatch.setattr(
        "fno.agents.dispatch.pick_account_id",
        lambda *a, **k: pytest.fail("picked an account for a revive"),
    )

    # Seam 1: the argv rewrite the CLI applies before either runtime.
    args = ["spawn", "router", "--resume", "sess-1", "--substrate", "bg"]
    assert rr._pick_account_at_seam(args) == args

    # Seam 2: the in-process caller that bypasses argument parsing. The restore
    # runs (proved by the refusal naming the missing file), and no pick fires.
    Path(path).unlink()
    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="router",
            message="go",
            provider="claude",
            cwd=tmp_path,
            resume_session_id="sess-1",
        )
    assert path in str(exc.value)
    assert "--account" not in str(exc.value), "no refusal about a flag nobody typed"


def test_a_restored_route_is_announced(tmp_path, monkeypatch, capsys) -> None:
    """A relaunch that changes destination without saying so is the failure this
    path exists to remove; a restore that says nothing is the same silence."""
    from fno.agents.dispatch import dispatch_spawn

    path = _routed_claude_row(tmp_path, monkeypatch)

    def _stop(**kw):
        raise RuntimeError(f"stop before launch; route={sorted(kw['route_env'] or {})}")

    monkeypatch.setattr("fno.agents.dispatch._claude_create_path", _stop)
    with pytest.raises(RuntimeError) as exc:
        dispatch_spawn(
            name="router",
            message="go",
            provider="claude",
            cwd=tmp_path,
            resume_session_id="sess-1",
        )
    assert "ANTHROPIC_BASE_URL" in str(exc.value), "the restored route reaches the launch"
    assert path in capsys.readouterr().err


def test_a_restored_route_pays_the_managed_oauth_composition_guard(
    tmp_path, monkeypatch
) -> None:
    """The restore goes THROUGH resolve_spawn_route, not past it.

    Managed OAuth owns the default Claude credential slot, so resolve_spawn_route
    refuses a foreign endpoint layered over it unless an account overlay is
    present and the route is self-authed. A restored route assigned past that
    call would be the one route in the system exempt from the check.
    """
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    _routed_claude_row(tmp_path, monkeypatch)
    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "claude-managed")
    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="router",
            message="go",
            provider="claude",
            cwd=tmp_path,
            resume_session_id="sess-1",
        )
    assert exc.value.exit_code == 2
    assert "managed OAuth" in str(exc.value)
    assert "no worker launched" in str(exc.value)

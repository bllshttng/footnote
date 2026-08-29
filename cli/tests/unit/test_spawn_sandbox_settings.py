"""Unit tests for the composed ``--settings`` sandbox payload (x-6ac1).

There is ONE ``--settings`` flag, so the sandbox write policy must compose
into the same file the route/auth floor already rides: a second file would
silently drop the first. Widening ``_write_settings_env_file`` to a full
payload keeps content addressing (a per-worker sandbox block naturally yields
a distinct digest) and the 0600 tmp-then-``os.replace`` mechanics.

The parity rule under test: a spawn with no write policy produces the SAME
settings path and argv as before this feature existed, and a route + policy
spawn keeps the route's credential keys byte-identical to a route-only spawn.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir

ROUTE_ENV = {
    "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "zai-secret-token",
    "ANTHROPIC_MODEL": "glm-5.2",
    # resolve_spawn_route refuses a pre-resolved route that is not a complete
    # unit, so the model tiers ride along like a real --route spawn's.
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-5.2",
}

SANDBOX = {
    "enabled": True,
    "filesystem": {"allowWrite": ["/wt/src/a.py", "/wt/.git/"]},
}


def test_settings_writer_widens_to_full_payload(tmp_path, monkeypatch):
    """Same env + no sandbox = same path as ever; a sandbox block = new digest."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.model_routing import _write_settings_env_file

    plain = Path(_write_settings_env_file(dict(ROUTE_ENV)))
    assert json.loads(plain.read_text()) == {"env": ROUTE_ENV}
    # Content addressing is over the whole payload: the identical call still
    # collapses to one file...
    assert Path(_write_settings_env_file(dict(ROUTE_ENV))) == plain
    # ...and a sandbox block yields a DIFFERENT file carrying both keys.
    compose = Path(_write_settings_env_file(dict(ROUTE_ENV), sandbox=SANDBOX))
    assert compose != plain
    payload = json.loads(compose.read_text())
    assert payload["env"] == ROUTE_ENV
    assert payload["sandbox"] == SANDBOX


def test_route_and_policy_compose_in_one_settings_file(tmp_path, monkeypatch):
    """AC7-EDGE: one --settings carries the route's keys plus the sandbox."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.model_routing import route_settings_path_for

    route_only = json.loads(
        Path(route_settings_path_for(dict(ROUTE_ENV), env={})).read_text()
    )
    both_path = route_settings_path_for(dict(ROUTE_ENV), env={}, sandbox=SANDBOX)
    both = json.loads(Path(both_path).read_text())
    # Credential keys byte-identical to a route-only spawn (compared over the
    # route's own keys: the ambient shell's unrouted-model floor may add ""
    # entries to both sides and is not this feature's concern)...
    assert {k: both["env"][k] for k in ROUTE_ENV} == {
        k: route_only["env"][k] for k in ROUTE_ENV
    }
    # ...plus the sandbox block in the SAME file.
    assert both["sandbox"] == SANDBOX


def test_policy_without_route_still_writes_the_scrub_floor(tmp_path, monkeypatch):
    """A sandbox-only spawn still owes the auth floor: it is the one channel
    that survives the daemon fork, sandbox or not."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.account_env import SCRUB_AUTH_VARS
    from fno.agents.model_routing import route_settings_path_for

    path = route_settings_path_for(None, None, env={}, sandbox=SANDBOX)
    assert path is not None
    payload = json.loads(Path(path).read_text())
    assert payload["sandbox"] == SANDBOX
    assert all(payload["env"][var] == "" for var in SCRUB_AUTH_VARS)


def test_no_policy_returns_no_settings_file(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.model_routing import route_settings_path_for

    assert route_settings_path_for(None, None, env={}, sandbox=None) is None


class _Capture(Exception):
    def __init__(self, argv):
        self.argv = argv


def test_bg_create_composes_one_settings_flag(tmp_path, monkeypatch):
    """Exactly one --settings; the policy composes into the SAME file the
    route rides, and the route's env is byte-identical to a route-only
    bg spawn's."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.harnesses import claude

    def fake_run(argv, **kwargs):
        raise _Capture(argv)

    monkeypatch.setattr(claude, "_subprocess_run", fake_run)

    with pytest.raises(_Capture) as plain:
        claude.bg_create(
            "w1", "hi", cwd=tmp_path, route_env=dict(ROUTE_ENV)
        )
    plain_argv = plain.value.argv
    assert plain_argv.count("--settings") == 1
    plain_settings = json.loads(
        Path(plain_argv[plain_argv.index("--settings") + 1]).read_text()
    )

    with pytest.raises(_Capture) as both:
        claude.bg_create(
            "w2", "hi", cwd=tmp_path, route_env=dict(ROUTE_ENV),
            sandbox_settings=SANDBOX,
        )
    both_argv = both.value.argv
    assert both_argv.count("--settings") == 1
    settings = json.loads(
        Path(both_argv[both_argv.index("--settings") + 1]).read_text()
    )
    # Same file shape, credential env unchanged, sandbox only in the second.
    assert settings["env"] == plain_settings["env"]
    assert "sandbox" not in plain_settings
    assert settings["sandbox"] == SANDBOX
    assert {k: settings["env"][k] for k in ROUTE_ENV} == ROUTE_ENV


def test_bg_create_without_policy_stays_byte_identical(tmp_path, monkeypatch):
    """AC6-EDGE parity: with a clean env (no route, no account, no policy,
    no incoherent model claims) the argv carries no --settings at all, and
    nothing sandbox-shaped appears under any input."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.harnesses import claude

    def fake_run(argv, **kwargs):
        raise _Capture(argv)

    monkeypatch.setattr(claude, "_subprocess_run", fake_run)
    monkeypatch.setattr(
        "fno.agents.model_routing.incoherent_model_env", lambda *a, **k: []
    )
    monkeypatch.setattr(
        "fno.agents.model_routing.unrouted_model_keys", lambda *a, **k: []
    )
    import os

    for key in [k for k in os.environ if k.startswith("ANTHROPIC_")]:
        monkeypatch.delenv(key, raising=False)

    with pytest.raises(_Capture) as unrouted:
        claude.bg_create("w3", "hi", cwd=tmp_path)
    assert "--settings" not in unrouted.value.argv


def test_load_sandbox_write_policy_reads_the_sandbox_block(tmp_path, monkeypatch):
    """The spawn CLI flag takes a policy FILE and fails closed on a bad one."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.model_routing import load_sandbox_write_policy

    good = tmp_path / "policy.json"
    good.write_text(json.dumps({"band": "high", "sandbox": SANDBOX}))
    assert load_sandbox_write_policy(good) == SANDBOX

    for bad_payload in ('{"band": "high"}', "not json"):
        bad = tmp_path / "bad.json"
        bad.write_text(bad_payload)
        with pytest.raises(SystemExit):
            load_sandbox_write_policy(bad)
    with pytest.raises(SystemExit):
        load_sandbox_write_policy(tmp_path / "missing.json")

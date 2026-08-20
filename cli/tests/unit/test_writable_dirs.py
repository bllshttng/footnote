"""fno's computed writable-directory grant: the set, and the three lanes that carry it.

The defect these cover: a spawned worker on a bounded posture cannot write
``~/.fno/claims``, so it holds no node claim and ``fno claim status`` answers
``free`` while it works - a duplicate-dispatch trap the standing "check the claim
first" rule cannot catch, because the check returns free.

One resolver, three funnels. Each lane gets its OWN assertion here rather than
one assertion standing in for the pair: a grant present on one of N reachable
paths is decorative.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.writable_dirs import worker_writable_dirs


@pytest.fixture
def fake_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point every state resolver at one existing tmp root."""
    state = tmp_path / "state"
    (state / "claims").mkdir(parents=True)
    monkeypatch.setattr("fno.paths.state_dir", lambda: state)
    monkeypatch.setattr("fno.claims.io.global_claims_root", lambda: state)
    monkeypatch.setattr("fno.claims.io.claims_dir", lambda root=None: state / "claims")
    # No plan directory unless a test opts in: an unresolvable plans dir grants
    # nothing, which keeps the state-root assertions below about the state root.
    monkeypatch.setattr(
        "fno.paths.plans_content_dir", lambda project_root=None: tmp_path / "nope"
    )
    return state


def test_state_root_is_always_granted(fake_state: Path, tmp_path: Path) -> None:
    assert worker_writable_dirs(tmp_path) == [str(fake_state)]


def test_state_root_and_divergent_claims_root_both_granted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``locks_dir``/``global_claims_root`` are deliberately config-free while
    ``state_dir`` honors ``config.paths.state_dir``. When an override splits
    them, a grant for one leaves the worker unable to write the other."""
    configured = tmp_path / "configured"
    configured.mkdir()
    claims_home = tmp_path / "home" / ".fno"
    (claims_home / "claims").mkdir(parents=True)
    monkeypatch.setattr("fno.paths.state_dir", lambda: configured)
    monkeypatch.setattr("fno.claims.io.global_claims_root", lambda: tmp_path / "home")
    monkeypatch.setattr(
        "fno.claims.io.claims_dir", lambda root=None: claims_home / "claims"
    )
    monkeypatch.setattr(
        "fno.paths.plans_content_dir", lambda project_root=None: tmp_path / "nope"
    )

    assert worker_writable_dirs(tmp_path) == [str(configured), str(claims_home)]


def test_plan_dir_is_granted_but_never_the_vault_above_it(
    fake_state: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The grant is a WRITE grant, so granting the vault root to let a worker
    write one plan hands a code worker the operator's whole notes directory.
    Grant the plan's own directory instead."""
    vault = tmp_path / "vault"
    plans = vault / "fno" / "plans"
    plans.mkdir(parents=True)
    monkeypatch.setattr("fno.paths.vault_root", lambda **kw: vault)

    got = worker_writable_dirs(tmp_path, plan_path=plans / "p.md")
    assert got == [str(fake_state), str(plans)]
    assert str(vault) not in got


def test_plan_dir_falls_back_to_the_configured_plans_dir(
    fake_state: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No caller passes plan_path today, so the fallback decides every real
    spawn. It must resolve to the plan DIRECTORY, not something above it."""
    plans = tmp_path / "repo" / "docs" / "plans"
    plans.mkdir(parents=True)
    monkeypatch.setattr(
        "fno.paths.plans_content_dir", lambda project_root=None: plans
    )
    assert worker_writable_dirs(tmp_path) == [str(fake_state), str(plans)]


def test_foreign_roots_ride_last_and_only_when_passed(
    fake_state: Path, tmp_path: Path
) -> None:
    sibling = tmp_path / "sibling"
    sibling.mkdir()
    assert worker_writable_dirs(tmp_path, foreign_roots=[sibling]) == [
        str(fake_state),
        str(sibling),
    ]
    assert worker_writable_dirs(tmp_path) == [str(fake_state)]


def test_missing_directory_is_not_granted(
    fake_state: Path, tmp_path: Path
) -> None:
    """A grant naming a directory that is not there is refused by some harnesses
    and buys nothing on any of them."""
    assert worker_writable_dirs(tmp_path, foreign_roots=[tmp_path / "nope"]) == [
        str(fake_state)
    ]


def test_set_is_deduplicated(fake_state: Path, tmp_path: Path) -> None:
    assert worker_writable_dirs(
        tmp_path, foreign_roots=[fake_state, fake_state]
    ) == [str(fake_state)]


# --------------------------------------------------------------------------
# The three funnels. One assertion per lane.
# --------------------------------------------------------------------------


@pytest.fixture
def one_grant(monkeypatch: pytest.MonkeyPatch) -> str:
    """Pin the computed set to one known token across all three funnels."""
    token = "/grant/state-root"
    for target in (
        "fno.agents.mux_spawn.worker_writable_dirs",
        "fno.agents.harnesses.claude.worker_writable_dirs",
        "fno.agents.writable_dirs.worker_writable_dirs",
    ):
        monkeypatch.setattr(target, lambda *a, **k: [token])
    return token


@pytest.mark.parametrize("provider", ["claude", "codex", "agy"])
def test_pane_lane_carries_the_grant(
    provider: str, one_grant: str, tmp_path: Path
) -> None:
    from fno.agents.mux_spawn import build_pane_argv

    argv = build_pane_argv(provider, "t", tmp_path, False, None)
    # Located by PAIR, not by the first --add-dir: codex's own git and plan
    # carveouts ride the same repeatable flag ahead of this one.
    pairs = [
        (argv[i], argv[i + 1]) for i, tok in enumerate(argv) if tok == "--add-dir"
    ]
    assert ("--add-dir", one_grant) in pairs


def test_codex_pane_grant_leaves_the_sandbox_flag_alone(
    one_grant: str, tmp_path: Path
) -> None:
    """The grant is additive. Widening the default posture to a bypass to make
    the claim write work was explicitly refused; the two bypass postures are
    opt-in on purpose."""
    from fno.agents.mux_spawn import build_pane_argv

    argv = build_pane_argv("codex", "t", tmp_path, False, None)
    assert argv[argv.index("--sandbox") + 1] == "workspace-write"
    assert one_grant in argv


def test_claude_bg_lane_carries_the_grant(one_grant: str, tmp_path: Path) -> None:
    from fno.agents.harnesses.claude import _build_argv

    argv = _build_argv(name="w", message="m", use_stdin=False, cwd=tmp_path)
    assert argv[argv.index("--add-dir") + 1] == one_grant


def test_claude_headless_lane_carries_the_grant(
    one_grant: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Asserted separately from the bg lane: they are two argv builders and a
    grant on one of them is decorative."""
    import fno.agents.harnesses.claude as claude_mod

    seen: dict[str, list[str]] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        seen["argv"] = list(argv)
        raise SystemExit(0)

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    with pytest.raises(SystemExit):
        claude_mod.headless_create(cwd=tmp_path, message="m")
    argv = seen["argv"]
    assert argv[:2] == ["claude", "-p"]
    assert argv[argv.index("--add-dir") + 1] == one_grant


def test_codex_headless_lane_carries_the_grant(
    one_grant: str, tmp_path: Path
) -> None:
    from fno.agents.writable_dirs import add_dir_tokens

    def boom(flag: str) -> object:
        raise AssertionError(flag)

    assert add_dir_tokens("codex", None, [one_grant], unsupported=boom) == [
        "--add-dir",
        one_grant,
    ]


# --------------------------------------------------------------------------
# The provider with no additive grant. Two directions, and they differ.
# --------------------------------------------------------------------------


def test_computed_set_is_skipped_on_a_provider_with_no_additive_grant(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Fail-closed is correct for an operator flag and catastrophic for a
    computed default: raising here would refuse every opencode spawn."""
    from fno.agents.mux_spawn import build_pane_argv

    argv = build_pane_argv("opencode", "t", tmp_path, False, None)
    assert "--add-dir" not in argv
    assert "--dir" not in argv
    err = capsys.readouterr().err
    assert "opencode" in err
    assert "claim" in err


def test_explicit_add_dir_still_refuses_on_that_provider(tmp_path: Path) -> None:
    """The computed-set skip must not weaken the operator-flag refusal."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import build_pane_argv

    with pytest.raises(DispatchAskError, match="not supported for provider"):
        build_pane_argv("opencode", "t", tmp_path, False, None, add_dir="/w")

"""`fno do target init` refuses a review gate this session cannot satisfy (x-cdc7).

PR #618 discovered its unsatisfiable `reviewers: [sigma]` gate at the stop gate,
after the feature was built and pushed, and then misreported it for fifteen
turns. The check under test moves that discovery to the front of the run.

The three outcomes must stay distinct. Collapsing `needs-operator` into
`unavailable` would teach an operator to "fix" a configuration that is correct
and merely needs a human, and collapsing either into a `declare` substitution
would produce a green gate with no review behind it.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import typer
from typer.testing import CliRunner

from fno.config import ReviewerDescriptor, load_settings
from fno.review_capability import (
    SessionCapability,
    detect_session,
    refusal_message,
    resolve_reviewers,
)
from fno.target_cli import REVIEW_GATE_REFUSED, _refuse_unsatisfiable_reviewers

CLAUDE_PANE = SessionCapability(harness="claude", substrate="interactive", attended=True)
CODEX_HEADLESS = SessionCapability(harness="codex", substrate="headless", attended=False)
# The incapable harness. Codex reaches the sigma panel via spawn_agent (with a
# sequential fallback that still attests), so gemini is the one that cannot.
GEMINI_HEADLESS = SessionCapability(harness="gemini", substrate="headless", attended=False)
CLAUDE_BG = SessionCapability(harness="claude", substrate="bg", attended=False)

# `code-review` was the only built-in on the operator path; it moved to
# self-serve (x-6d9c), so the needs-operator branch has no built-in left. This
# registry-only reviewer keeps that branch under test without re-introducing a
# built-in whose whole point was that a session cannot run it unattended.
OPERATOR_REVIEWER = {
    "operator-only": ReviewerDescriptor(
        kind="human",
        requires="operator",
        invocation="/human-review",
        asserts="review-evidence",
    )
}


def _config(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, reviewers: str) -> None:
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(f"schema_version: 1\nconfig:\n  review:\n    reviewers: {reviewers}\n")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()


@pytest.fixture(autouse=True)
def _clear_cache():
    yield
    load_settings.cache_clear()


# --- AC1: refuse at init -----------------------------------------------------


def test_refusal_names_reviewer_capability_harness_substrate_and_remedies():
    verdicts = resolve_reviewers(["sigma"], GEMINI_HEADLESS)
    msg = refusal_message(verdicts, GEMINI_HEADLESS)
    assert msg is not None
    assert "sigma" in msg
    assert "subagent-dispatch" in msg
    assert "harness=gemini" in msg
    assert "substrate=headless" in msg
    assert "change config.review.reviewers" in msg
    # The remedy must be reachable. `sigma` here is `unavailable`, and the
    # subagent-dispatch branch never reads session.attended, so "run attended"
    # would be a remedy that provably cannot clear this gate.
    assert "run attended" not in msg
    assert "emit-attestation.sh" in msg


def test_needs_operator_keeps_the_run_attended_remedy():
    """The other half of the split: attendedness IS the fix for an operator
    reviewer, so that remedy must survive."""
    verdicts = resolve_reviewers(["operator-only"], CLAUDE_BG, registry=OPERATOR_REVIEWER)
    msg = refusal_message(verdicts, CLAUDE_BG)
    assert msg is not None
    assert "run attended" in msg
    assert "emit-attestation.sh" not in msg


def test_init_exits_non_zero_before_touching_the_init_script(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    _config(tmp_path, monkeypatch, "[sigma]")
    monkeypatch.setenv("GEMINI_SESSION_ID", "g1")
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    monkeypatch.setenv("TARGET_UNATTENDED", "1")
    with pytest.raises(typer.Exit) as exc:
        _refuse_unsatisfiable_reviewers()
    assert exc.value.exit_code == 2


def test_satisfiable_reviewer_does_not_refuse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    _config(tmp_path, monkeypatch, "[sigma]")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "s1")
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    _refuse_unsatisfiable_reviewers()  # no raise


def test_empty_reviewers_is_a_no_op(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    _config(tmp_path, monkeypatch, "[]")
    monkeypatch.setenv("GEMINI_SESSION_ID", "g1")
    _refuse_unsatisfiable_reviewers()  # no raise


def test_init_refuses_identity_free_peers_when_all_are_same_model(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    cfg = tmp_path / "config.toml"
    cfg.write_text('[review]\npeers = ["codex"]\n')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("CODEX_THREAD_ID", "c1")
    load_settings.cache_clear()
    with pytest.raises(typer.Exit) as exc:
        _refuse_unsatisfiable_reviewers()
    assert exc.value.exit_code == 2


def test_init_allows_identity_free_peers_with_cross_model_option(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[review]\npeers = ["codex", '
        '{provider = "claude", model = "zai,glm-5.2"}]\n'
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("CODEX_THREAD_ID", "c1")
    load_settings.cache_clear()
    _refuse_unsatisfiable_reviewers()


def test_unreadable_config_degrades_to_a_no_op(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
):
    """A broken probe must never be the reason bootstrap cannot run - and must
    never be silent about having degraded. Asserting only "no raise" would pass
    against the swallow this replaced."""
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: (_ for _ in ()).throw(RuntimeError("boom"))
    )
    _refuse_unsatisfiable_reviewers()  # no raise
    assert "review capability check skipped" in capsys.readouterr().err


# --- AC4: correctly-unsatisfiable is not misconfigured -----------------------


def test_operator_reviewer_unattended_is_needs_operator_not_unavailable():
    (v,) = resolve_reviewers(["operator-only"], CLAUDE_BG, registry=OPERATOR_REVIEWER)
    assert v.status == "needs-operator"
    assert v.descriptor.kind == "human"
    assert "Not a misconfiguration" in v.reason


def test_operator_reviewer_attended_is_satisfiable():
    (v,) = resolve_reviewers(["operator-only"], CLAUDE_PANE, registry=OPERATOR_REVIEWER)
    assert v.status == "satisfiable"


def test_code_review_is_self_servable_even_unattended():
    """AC9-UI: the self-review reviewer is self-servable, so an unattended run
    is not refused at init for naming it. The operator path it used to live on
    is covered by the operator-only fixture above."""
    for session in (CLAUDE_BG, CODEX_HEADLESS, CLAUDE_PANE):
        (v,) = resolve_reviewers(["code-review"], session)
        assert v.status == "satisfiable", session
        assert v.descriptor.requires == "none"
        assert v.blocks_autonomy is False


def test_unknown_reviewer_is_distinct_from_both():
    (v,) = resolve_reviewers(["teleport"], CLAUDE_PANE)
    assert v.status == "unavailable"
    assert "unknown reviewer" in v.reason


def test_needs_operator_still_blocks_an_unattended_run():
    """Distinct from unavailable in wording, identical in consequence: an
    unattended run with a human reviewer wedges exactly like #618."""
    verdicts = resolve_reviewers(["operator-only"], CLAUDE_BG, registry=OPERATOR_REVIEWER)
    assert refusal_message(verdicts, CLAUDE_BG) is not None


# --- AC5: declare is never chosen for you ------------------------------------


def test_refusal_never_proposes_declare_as_the_fix():
    msg = refusal_message(resolve_reviewers(["sigma"], GEMINI_HEADLESS), GEMINI_HEADLESS)
    assert msg is not None
    assert "declare" not in msg


def test_declare_prints_marked_as_self_cert():
    (v,) = resolve_reviewers(["declare"], GEMINI_HEADLESS)
    assert v.status == "satisfiable"
    assert "self-cert" in v.line()
    assert "asserts no review evidence" in v.line()


def test_unavailable_reviewer_is_not_swapped_for_declare():
    verdicts = resolve_reviewers(["sigma"], GEMINI_HEADLESS)
    assert [v.name for v in verdicts] == ["sigma"]
    assert verdicts[0].status == "unavailable"


# --- session detection -------------------------------------------------------


def test_detect_session_reads_harness_and_substrate():
    env = {"CLAUDE_CODE_SESSION_ID": "s1", "FNO_BG": "1"}
    s = detect_session(env)
    assert (s.harness, s.substrate, s.attended) == ("claude", "bg", False)


def test_unclassifiable_session_is_unverifiable_not_unavailable():
    """A plain terminal has no harness marker. Refusing it would break
    `fno do target init` outright in every repo that configures a reviewer, and it
    is not the same claim as "this harness cannot dispatch subagents"."""
    s = detect_session({})
    assert s.harness == "unknown"
    (v,) = resolve_reviewers(["sigma"], s)
    assert v.status == "unverifiable"
    assert v.blocks_autonomy is False
    assert "cannot be verified" in v.reason
    assert refusal_message([v], s) is None


def test_known_incapable_harness_still_refuses():
    """AC1 is about a session that demonstrably cannot, not one we cannot read."""
    (v,) = resolve_reviewers(["sigma"], GEMINI_HEADLESS)
    assert v.status == "unavailable"
    assert v.blocks_autonomy is True


def test_unverifiable_session_does_not_refuse_init(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    _config(tmp_path, monkeypatch, "[sigma]")
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    _refuse_unsatisfiable_reviewers()  # no raise


# --- the capability check must not be the thing that silently does not fire ---


def test_unknown_reviewer_does_not_borrow_declares_descriptor():
    """A name absent from the table once rendered with `declare`'s descriptor,
    so an unknown reviewer reported itself as a self-cert that satisfies the
    gate - the exact opposite of its own `unavailable` verdict."""
    (v,) = resolve_reviewers(["teleport"], CLAUDE_PANE)
    assert v.status == "unavailable"
    assert v.descriptor is None
    assert "self-cert" not in v.line()
    assert "/fno:review declare" not in v.line()


def test_config_unattended_reaches_the_operator_verdict():
    """`attended` has two inputs in the manifest (TARGET_UNATTENDED OR
    config.unattended.enabled) and had only the env one here, so a
    config-unattended run reported an operator reviewer as satisfiable and
    wedged at the stop gate instead."""
    s = detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}, unattended_configured=True)
    assert s.attended is False
    (v,) = resolve_reviewers(["operator-only"], s, registry=OPERATOR_REVIEWER)
    assert v.status == "needs-operator"
    assert v.blocks_autonomy is True


def test_detect_session_reads_config_unattended(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The config read is wired, not just the parameter."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text("schema_version: 1\nconfig:\n  unattended:\n    enabled: true\n")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is False

    cfg.write_text("schema_version: 1\nconfig:\n  unattended:\n    enabled: false\n")
    assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is True


def test_config_unattended_uses_the_shells_notion_of_true(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """`hooks/helpers/init-target-state.sh` compares get_config output to the
    STRING "true", so a quoted or numeric value makes the MANIFEST unattended.
    `is True` here answered attended, which clears an `operator` reviewer the
    run then cannot satisfy - the #618 class, silently."""
    cfg = tmp_path / "settings.yaml"
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    for value in ('"true"', "'yes'", "1", "on", "TRUE"):
        cfg.write_text(
            f"schema_version: 1\nconfig:\n  unattended:\n    enabled: {value}\n"
        )
        assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is False, value
    for value in ('"false"', "0", "no", "nonsense", "[]"):
        cfg.write_text(
            f"schema_version: 1\nconfig:\n  unattended:\n    enabled: {value}\n"
        )
        assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is True, value


def test_unattended_empty_value_falls_through_like_the_shell(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """`scripts/lib/config.sh` skips a candidate whose value is empty or null
    and reads the NEXT layer. Stopping at the first mention would report
    attended for a config the manifest reads as unattended - the fail-open
    direction.

    Two layers on purpose: with a single candidate the fall-through is
    unobservable, so a one-layer test passes with or without the skip and pins
    nothing.
    """
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)

    local = tmp_path / "proj" / ".fno"
    local.mkdir(parents=True)
    home = tmp_path / "home" / ".fno"
    home.mkdir(parents=True)
    (home / "settings.yaml").write_text(
        "schema_version: 1\nconfig:\n  unattended:\n    enabled: true\n"
    )
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.chdir(tmp_path / "proj")

    for value in ('""', "null"):
        (local / "settings.yaml").write_text(
            f"schema_version: 1\nconfig:\n  unattended:\n    enabled: {value}\n"
        )
        assert (
            detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is False
        ), f"an empty local value must fall through to the global true ({value})"

    # A real value in the local layer wins and does NOT fall through.
    (local / "settings.yaml").write_text(
        "schema_version: 1\nconfig:\n  unattended:\n    enabled: false\n"
    )
    assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is True


def test_unattended_table_without_enabled_falls_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The per-KEY guard is load-bearing, not defensive: without it this raises
    KeyError, and the resolution path is deliberately NOT swallowed, so the
    traceback escapes `fno do target init`."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text("schema_version: 1\nconfig:\n  unattended:\n    reason: ci\n")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is True


def test_unreadable_reviewers_config_is_reported_never_silent(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
):
    """A typo'd reviewer name raises out of the config validator. Swallowing it
    skipped the refusal in total silence while the Rust stop gate went live on a
    name no attestation can match - the wedge this check exists to prevent."""
    import fno.config as cfg_mod

    def _boom():
        raise ValueError("names an unresolvable reviewer 'sigmma'")

    monkeypatch.setattr(cfg_mod, "load_settings", _boom)
    _refuse_unsatisfiable_reviewers()  # degrades, but must not be silent
    err = capsys.readouterr().err
    assert "review capability check skipped" in err
    assert "sigmma" in err


def test_resolution_errors_are_not_swallowed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """Only the config read degrades. Once a reviewer is known to be configured,
    guessing "available" is the wedge, so a failing probe must propagate."""
    import fno.review_capability as rc

    _config(tmp_path, monkeypatch, "[sigma]")

    def _boom() -> None:
        raise RuntimeError("probe died")

    monkeypatch.setattr(rc, "detect_session", _boom)
    with pytest.raises(RuntimeError, match="probe died"):
        _refuse_unsatisfiable_reviewers()


def test_degraded_attendedness_probe_announces_its_guess(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
):
    """The probe guesses "attended" when the config cannot be located, which is
    the safe direction but also the one that lets an operator reviewer look
    satisfiable. Guessing silently is how the run reaches the stop gate and
    wedges, so the guess is announced."""
    import fno.review_capability as rc

    def _boom() -> list[Path]:
        raise RuntimeError("no HOME")

    monkeypatch.setattr(rc, "_settings_yaml_locations", _boom, raising=False)
    monkeypatch.setattr(
        "fno.config._settings_yaml_locations", _boom, raising=False
    )
    assert rc._unattended_in_config() is False
    err = capsys.readouterr().err
    assert "attendedness probe degraded" in err
    assert "assuming attended" in err


# --- the call site itself, not just the helper --------------------------------
#
# Every test above imports `_refuse_unsatisfiable_reviewers` directly. A mutation
# run proved that is not enough: replacing the single call in `init` with `pass`,
# and separately deleting both `typer.echo` calls, each left the whole suite
# green. These drive the real command so the wiring and the printing are pinned.


def _invoke_init(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, reviewers: str, env: dict):
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(f"schema_version: 1\nconfig:\n  review:\n    reviewers: {reviewers}\n")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    for k, v in env.items():
        monkeypatch.setenv(k, v)
    load_settings.cache_clear()
    from fno.cli import app

    return CliRunner().invoke(app, ["do", "target", "init", "--input", "some-feature"])


def test_init_command_refuses_and_prints_the_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC1 end to end: the refusal must reach stderr from the real command, not
    merely be returned by `refusal_message`."""
    r = _invoke_init(
        tmp_path, monkeypatch, "[sigma]",
        {"GEMINI_SESSION_ID": "g1", "TARGET_UNATTENDED": "1"},
    )
    assert r.exit_code == 2
    assert "subagent-dispatch" in r.output
    assert "sigma" in r.output
    assert "harness=gemini" in r.output
    assert "substrate=headless" in r.output
    assert "change config.review.reviewers" in r.output


def test_init_command_refuses_before_the_bootstrap_script_runs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The check must sit ahead of `_resolve_init_script()`. Point the plugin
    root at an empty dir: if the check ran second, the missing-script refusal
    (exit 2, a different message) would win instead."""
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path / "empty-plugin-root"))
    r = _invoke_init(
        tmp_path, monkeypatch, "[sigma]",
        {"GEMINI_SESSION_ID": "g1", "TARGET_UNATTENDED": "1"},
    )
    assert r.exit_code == 2
    assert "subagent-dispatch" in r.output
    assert "needs the footnote plugin" not in r.output


def test_init_command_prints_the_unverifiable_note(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """An unclassifiable session proceeds, but never silently."""
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path / "empty-plugin-root"))
    r = _invoke_init(tmp_path, monkeypatch, "[sigma]", {})
    assert "note target init:" in r.output
    assert "cannot be verified" in r.output
    # It got past the reviewer check and died on the absent plugin root instead.
    assert "needs the footnote plugin" in r.output


def test_init_command_writes_no_state_when_it_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """"No work begins" is the half of AC1 the message assertions cannot see:
    a refusal that still stamped a manifest would leave a claimed node behind."""
    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    monkeypatch.chdir(repo)

    r = _invoke_init(
        tmp_path, monkeypatch, "[sigma]",
        {"GEMINI_SESSION_ID": "g1", "TARGET_UNATTENDED": "1"},
    )

    assert r.exit_code == 2
    assert not (repo / ".fno" / "target-state.md").exists()


def test_codex_can_satisfy_a_sigma_gate():
    """Codex reaches the sigma panel through project custom agents /
    `spawn_agent`, and a surface without that primitive reports the downgrade
    and runs the panel sequentially - slower, but it still reaches a verdict and
    still attests (docs/HARNESSES.md, docs/SKILL-COMPAT-MATRIX.md).

    Treating codex as incapable hard-exits `fno do target init` on a configuration
    the project documents as supported: a false refusal, which is a worse
    failure than the late wedge this check exists to prevent."""
    (v,) = resolve_reviewers(["sigma"], CODEX_HEADLESS)
    assert v.status == "satisfiable"
    assert v.blocks_autonomy is False
    assert refusal_message([v], CODEX_HEADLESS) is None


# --- AC12: github_apps axis (x-b167) -----------------------------------------
#
# The reviewers gate above refuses a LOCAL reviewer that cannot run here. The
# github_apps gate had no init-time equivalent, so a typo or an uninstalled App
# wedged the session at the budget ceiling instead. These pin the symmetric
# refusal on the App-bot axis: fail loud at init, fail-open on an unrunnable
# probe, and never on a login footnote recognizes.

from fno.review_capability import (  # noqa: E402
    github_apps_refusal_message,
    resolve_github_apps,
)
_NEVER = lambda login, cwd=None: False  # noqa: E731 - a probe that found nothing
_SEEN = lambda login, cwd=None: True  # noqa: E731 - a probe that found activity
_UNVERIFIABLE = lambda login, cwd=None: None  # noqa: E731 - a probe that could not run


def test_github_apps_recognized_login_needs_no_probe():
    """A login footnote knows (chatgpt-codex-connector, even before it has acted
    on a fresh repo) is satisfiable without ever running the probe."""

    def _explode(login, cwd=None):
        raise AssertionError("recognized login must not be probed")

    (v,) = resolve_github_apps(["chatgpt-codex-connector"], probe=_explode)
    assert v.status == "satisfiable"
    assert v.blocks_autonomy is False
    assert github_apps_refusal_message([v]) is None


def test_github_apps_typo_never_seen_is_refused_naming_optional_apps():
    """AC12: a login absent from the profiles and never seen in the repo is a
    likely typo. Refuse, naming the login and config.review.optional_apps."""
    (v,) = resolve_github_apps(["typo-bot-name"], probe=_NEVER)
    assert v.status == "unavailable"
    assert v.blocks_autonomy is True
    msg = github_apps_refusal_message([v])
    assert msg is not None
    assert "typo-bot-name" in msg
    assert "config.review.optional_apps" in msg


def test_github_apps_configured_nudge_login_is_satisfiable_without_probe():
    """A login the operator gave an explicit [review.nudge] profile is a named
    real App, not a typo, so it is satisfiable at init even with no prior
    activity - the documented first-use path for a custom mention-triggered App."""

    def _explode(login, cwd=None):
        raise AssertionError("a configured nudge login must not be probed")

    (v,) = resolve_github_apps(
        ["some-new-bot"], probe=_explode, nudge_logins=("some-new-bot",)
    )
    assert v.status == "satisfiable"
    assert github_apps_refusal_message([v]) is None


def test_github_apps_suffixed_typo_of_known_login_is_probed_not_recognized():
    """A known login with EXTRA suffix (chatgpt-codex-connector-typo) can never be
    satisfied by a real codex review (the Rust check is directional), so it must
    be probed and refused as a typo, not waved through as recognized."""
    (v,) = resolve_github_apps(["chatgpt-codex-connector-typo"], probe=_NEVER)
    assert v.status == "unavailable"
    assert v.blocks_autonomy is True


def test_github_apps_short_alias_of_known_login_is_recognized():
    """A genuine short alias (codex) IS recognized without a probe."""

    def _explode(login, cwd=None):
        raise AssertionError("a short alias must not be probed")

    (v,) = resolve_github_apps(["codex"], probe=_explode)
    assert v.status == "satisfiable"


def test_github_apps_unknown_but_seen_is_satisfiable():
    """A login footnote does not recognize but that HAS commented/reviewed here
    is a real bot, not a typo - proceed."""
    (v,) = resolve_github_apps(["some-real-bot"], probe=_SEEN)
    assert v.status == "satisfiable"
    assert github_apps_refusal_message([v]) is None


def test_github_apps_unverifiable_probe_proceeds():
    """A probe that could not run (no repo remote / token scope) is unverifiable,
    which proceeds - never refuse on a probe we could not run (section 6)."""
    (v,) = resolve_github_apps(["some-bot"], probe=_UNVERIFIABLE)
    assert v.status == "unverifiable"
    assert v.blocks_autonomy is False
    assert github_apps_refusal_message([v]) is None


def _invoke_init_apps(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, github_apps: str, env: dict
):
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(
        f"schema_version: 1\nconfig:\n  review:\n    github_apps: {github_apps}\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    for k, v in env.items():
        monkeypatch.setenv(k, v)
    load_settings.cache_clear()
    from fno.cli import app

    return CliRunner().invoke(app, ["do", "target", "init", "--input", "some-feature"])


def test_init_command_refuses_a_typo_github_app(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC12 end to end: a typo'd github_apps login never seen in the repo exits
    non-zero from the real command, before any bootstrap script runs."""
    import fno.review_capability as rc

    monkeypatch.setattr(rc, "_app_ever_acted", _NEVER)
    r = _invoke_init_apps(tmp_path, monkeypatch, "[typo-bot-name]", {})
    assert r.exit_code == 2
    assert "typo-bot-name" in r.output
    assert "config.review.optional_apps" in r.output


def test_init_command_allows_a_recognized_github_app(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """A recognized App must pass the github_apps check (and then fail later on
    the absent plugin root, proving it got past this gate, not that it refused)."""
    import fno.review_capability as rc

    def _explode(login, cwd=None):
        raise AssertionError("recognized login must not be probed")

    monkeypatch.setattr(rc, "_app_ever_acted", _explode)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(tmp_path / "empty-plugin-root"))
    r = _invoke_init_apps(tmp_path, monkeypatch, "[chatgpt-codex-connector]", {})
    assert "config.review.github_apps names a bot" not in r.output


# --- x-4a60: the same gate, reachable from the direct bash path ---------------
#
# Both refusals above were called only from the Python `init` wrapper. The
# script SKILL.md documents running directly (`TARGET_START=1 bash
# hooks/helpers/init-target-state.sh`) never ran them, so a typo'd login started
# a full run and surfaced at the stop gate. `check-review-gate` is how bash
# reaches the one implementation; `FNO_TARGET_INIT_GATED` is how the wrapper
# path says it already paid for it.


def _invoke_gate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, review_block: str,
                 env: dict | None = None):
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(f"schema_version: 1\nconfig:\n  review:\n{review_block}")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID", "CODEX_THREAD_ID",
                "CODEX_SESSION_ID", "GEMINI_SESSION_ID", "TARGET_UNATTENDED",
                "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    for k, v in (env or {}).items():
        monkeypatch.setenv(k, v)
    load_settings.cache_clear()
    from fno.cli import app

    return CliRunner().invoke(app, ["do", "target", "check-review-gate"])


def test_check_review_gate_is_silent_and_zero_on_an_empty_config(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC6-HP. Both refusals return early on an empty list, so a repo that
    configures no review at all pays nothing and hears nothing."""
    r = _invoke_gate(tmp_path, monkeypatch, "    reviewers: []\n    github_apps: []\n")
    assert r.exit_code == 0
    assert r.output.strip() == ""


def test_check_review_gate_refuses_an_unsatisfiable_reviewer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The verb re-stamps the wrapper's exit 2 as its own refusal code, which is
    the only code the shell caller treats as a refusal."""
    r = _invoke_gate(
        tmp_path, monkeypatch, "    reviewers: [sigma]\n",
        {"GEMINI_SESSION_ID": "g1", "TARGET_UNATTENDED": "1"},
    )
    assert r.exit_code == REVIEW_GATE_REFUSED
    assert "sigma" in r.output
    assert "change config.review.reviewers" in r.output


def test_check_review_gate_refuses_a_typo_github_app(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The App axis reaches the verb too - it is the axis PR #638 added and the
    one the direct path never checked."""
    import fno.review_capability as rc

    monkeypatch.setattr(rc, "_app_ever_acted", _NEVER)
    r = _invoke_gate(tmp_path, monkeypatch, "    github_apps: [typo-bot-name]\n")
    assert r.exit_code == REVIEW_GATE_REFUSED
    assert "typo-bot-name" in r.output


def test_check_review_gate_checks_apps_even_when_reviewers_pass(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """Ordering pin: a satisfiable reviewer must not short-circuit the App axis.
    Returning after the first clean verdict would restore half the gap."""
    import fno.review_capability as rc

    monkeypatch.setattr(rc, "_app_ever_acted", _NEVER)
    r = _invoke_gate(
        tmp_path, monkeypatch,
        "    reviewers: [sigma]\n    github_apps: [typo-bot-name]\n",
        {"CLAUDE_CODE_SESSION_ID": "s1"},
    )
    assert r.exit_code == REVIEW_GATE_REFUSED
    assert "typo-bot-name" in r.output


def test_init_marks_the_gate_as_already_run_for_the_script(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC3-CON, wrapper half: `init` hands the script FNO_TARGET_INIT_GATED=1,
    so the script skips a gate pass that already happened. Without it the App
    probe pays two 30s-timeout `gh` calls per unrecognized login twice."""
    captured: dict = {}

    class _Proc:
        returncode = 0

    def _fake_run(cmd, **kwargs):
        if "env" in kwargs:
            captured["env"] = kwargs["env"]
        return _Proc()

    monkeypatch.setattr("fno.target_cli.subprocess.run", _fake_run)
    monkeypatch.setattr("fno.target_cli._print_orientation_report", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_dispatch_work_start", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_reconcile_lane_slot", lambda *a, **k: None)
    monkeypatch.setattr("fno.target_cli._maybe_check_resume_receipt", lambda *a, **k: None)

    r = _invoke_init(tmp_path, monkeypatch, "[]", {"CLAUDE_CODE_SESSION_ID": "s1"})

    assert r.exit_code == 0, r.output
    assert captured["env"].get("FNO_TARGET_INIT_GATED") == "1"


def test_the_script_reads_the_marker_this_module_writes():
    """AC3-CON, cross-language half. The wrapper test above and the shell test in
    tests/hooks/test_init_review_gate_direct.sh each assert their own side's
    literal, so renaming ONE side leaves both green and silently restores the
    double probe. This is the only assertion that sees both."""
    import fno.target_cli as tc

    repo_root = Path(tc.__file__).resolve().parents[3]
    script = repo_root / "hooks" / "helpers" / "init-target-state.sh"
    if not script.is_file():  # bare `pip install fno` ships no hooks/
        pytest.skip("plugin hooks/ not present in this install")
    text = script.read_text(encoding="utf-8")

    # Anchored to the EXECUTABLE forms, not the bare names. Both names also
    # occur in this block's comments and in its note string, so a whole-file
    # grep for them stays green after the invocation is deleted outright -
    # which is exactly what a mutation run proved.
    assert '"${FNO_TARGET_INIT_GATED:-}" != "1"' in text, (
        "init-target-state.sh must still READ the marker, not merely mention it"
    )
    assert "fno do target check-review-gate && _RG_RC=0" in text, (
        "init-target-state.sh must still INVOKE the gate, not merely name it"
    )
    # And the NUMERAL. Bash cannot import the constant, so it restates it as a
    # literal; without this, changing REVIEW_GATE_REFUSED leaves both suites
    # green (these tests use the symbol, the shell test stubs its own exit code)
    # while every real refusal falls into the note-and-proceed branch instead.
    assert f"-eq {REVIEW_GATE_REFUSED} ]]" in text, (
        f"init-target-state.sh must test for -eq {REVIEW_GATE_REFUSED}; "
        "the shell literal and REVIEW_GATE_REFUSED have drifted apart"
    )


def test_refusal_code_is_one_click_can_never_produce(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The regression a real run found. Click exits 2 on UsageError, so an
    installed `fno` predating this verb answers `No such command
    'check-review-gate'` with exit 2. If the refusal spoke 2 as well, the shell
    caller could not tell them apart and every direct bootstrap would hard-refuse
    the moment the CLI fell behind source - a broken gate bricking the mandatory
    path, which is exactly what the fail-open rule forbids."""
    from fno.cli import app

    assert REVIEW_GATE_REFUSED not in (0, 1, 2)

    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "absent.yaml"))
    load_settings.cache_clear()
    stale = CliRunner().invoke(app, ["do", "target", "no-such-verb-x4a60"])
    assert stale.exit_code == 2
    assert stale.exit_code != REVIEW_GATE_REFUSED


def test_init_still_speaks_exit_2_on_a_refusal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC2-HP. The verb's private exit code must not leak into `fno do target
    init`, whose refusal contract is byte-for-byte unchanged."""
    r = _invoke_init(
        tmp_path, monkeypatch, "[sigma]",
        {"GEMINI_SESSION_ID": "g1", "TARGET_UNATTENDED": "1"},
    )
    assert r.exit_code == 2

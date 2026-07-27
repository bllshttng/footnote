"""`fno target init` refuses a review gate this session cannot satisfy (x-cdc7).

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

from fno.config import load_settings
from fno.review_capability import (
    SessionCapability,
    detect_session,
    refusal_message,
    resolve_reviewers,
)
from fno.target_cli import _refuse_unsatisfiable_reviewers

CLAUDE_PANE = SessionCapability(harness="claude", substrate="interactive", attended=True)
CODEX_HEADLESS = SessionCapability(harness="codex", substrate="headless", attended=False)
# The incapable harness. Codex reaches the sigma panel via spawn_agent (with a
# sequential fallback that still attests), so gemini is the one that cannot.
GEMINI_HEADLESS = SessionCapability(harness="gemini", substrate="headless", attended=False)
CLAUDE_BG = SessionCapability(harness="claude", substrate="bg", attended=False)


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
    verdicts = resolve_reviewers(["code-review"], CLAUDE_BG)
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


def test_human_reviewer_unattended_is_needs_operator_not_unavailable():
    (v,) = resolve_reviewers(["code-review"], CLAUDE_BG)
    assert v.status == "needs-operator"
    assert v.descriptor.kind == "human"
    assert "Not a misconfiguration" in v.reason


def test_human_reviewer_attended_is_satisfiable():
    (v,) = resolve_reviewers(["code-review"], CLAUDE_PANE)
    assert v.status == "satisfiable"


def test_unknown_reviewer_is_distinct_from_both():
    (v,) = resolve_reviewers(["teleport"], CLAUDE_PANE)
    assert v.status == "unavailable"
    assert "unknown reviewer" in v.reason


def test_needs_operator_still_blocks_an_unattended_run():
    """Distinct from unavailable in wording, identical in consequence: an
    unattended run with a human reviewer wedges exactly like #618."""
    verdicts = resolve_reviewers(["code-review"], CLAUDE_BG)
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
    `fno target init` outright in every repo that configures a reviewer, and it
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
    (v,) = resolve_reviewers(["code-review"], s)
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
    and reads the next layer. Stopping here instead would report attended for a
    config the manifest reads as unattended - the fail-open direction."""
    cfg = tmp_path / "settings.yaml"
    for value in ('""', "null"):
        cfg.write_text(
            f"schema_version: 1\nconfig:\n  unattended:\n    enabled: {value}\n"
        )
        monkeypatch.setenv("FNO_CONFIG", str(cfg))
        for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
            monkeypatch.delenv(var, raising=False)
        assert detect_session({"CLAUDE_CODE_SESSION_ID": "s1"}).attended is True, value


def test_unattended_table_without_enabled_falls_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The per-KEY guard is load-bearing, not defensive: without it this raises
    KeyError, and the resolution path is deliberately NOT swallowed, so the
    traceback escapes `fno target init`."""
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

    return CliRunner().invoke(app, ["target", "init", "--input", "some-feature"])


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

    Treating codex as incapable hard-exits `fno target init` on a configuration
    the project documents as supported: a false refusal, which is a worse
    failure than the late wedge this check exists to prevent."""
    (v,) = resolve_reviewers(["sigma"], CODEX_HEADLESS)
    assert v.status == "satisfiable"
    assert v.blocks_autonomy is False
    assert refusal_message([v], CODEX_HEADLESS) is None

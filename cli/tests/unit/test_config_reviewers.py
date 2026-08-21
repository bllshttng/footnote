"""Tests for the `_RESOLVABLE_REVIEWERS` descriptor table (node x-cdc7).

The table was a bare name set. A name answers "is this reviewer spelled
correctly"; it cannot answer "can this reviewer actually run, here", which is
the question that let a `reviewers: [sigma]` gate go unsatisfiable-by-
construction on PR #618 and surface only at the stop gate, after the work.

Promoting the set to a table must not change what the validator accepts or the
message it prints on a typo - that is a separate, already-tested contract
(test_config_review.py). These tests pin the new value shape, and the one rule
that must never soften: `declare` is visibly a self-certification.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.config import (
    _RESOLVABLE_REVIEWERS,
    ReviewerDescriptor,
    load_settings,
    resolvable_reviewers,
)


def _settings(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, body: str):
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(body)
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()
    return load_settings()


def test_every_entry_is_a_descriptor():
    assert set(_RESOLVABLE_REVIEWERS) == {"sigma", "code-review", "declare"}
    for name, d in _RESOLVABLE_REVIEWERS.items():
        assert isinstance(d, ReviewerDescriptor), name
        assert d.kind in {
            "local-attestation",
            "github-app",
            "external-cli",
            "human",
            "harness-skill",
        }
        assert d.requires in {"none", "subagent-dispatch", "operator", "skill"}
        assert d.invocation.strip(), f"{name} must name how to satisfy the gate"


def test_builtins_stay_bool_encodable():
    """AC9-INV: every built-in asserts review-evidence or self-cert.

    The Rust table encodes each reviewer as (name, invocation, is_self_cert).
    That bool is only a faithful encoding while the built-in axis has exactly
    two values; the `invocation` rung is registry-only for precisely this
    reason, and check-reviewer-descriptor-parity.sh reads the same two-valued
    axis out of this literal.
    """
    for name, d in _RESOLVABLE_REVIEWERS.items():
        assert d.asserts in {"review-evidence", "self-cert"}, (
            f"{name} asserts {d.asserts!r}; the `invocation` rung is "
            f"registry-only or the Rust bool encoding stops being faithful"
        )


def test_declare_is_the_only_self_cert():
    """AC5: `declare` satisfies the gate while asserting nothing, so every
    surface that prints it can tell that apart from a real review."""
    self_cert = {n for n, d in _RESOLVABLE_REVIEWERS.items() if d.asserts == "self-cert"}
    assert self_cert == {"declare"}
    assert _RESOLVABLE_REVIEWERS["declare"].requires == "none"


def test_sigma_needs_subagent_dispatch():
    """The capability that was missing on #618, now declared instead of implied."""
    assert _RESOLVABLE_REVIEWERS["sigma"].requires == "subagent-dispatch"
    assert _RESOLVABLE_REVIEWERS["sigma"].asserts == "review-evidence"


def test_code_review_is_self_servable():
    """`/code-review` is self-servable: a session that wrote the diff runs its
    own harness's review verb (claude /code-review, codex /review, opencode
    /review-changes, agy /fno:review) and attests. requires=none (not
    operator) is what keeps an unattended run from being refused at init for
    naming it."""
    d = _RESOLVABLE_REVIEWERS["code-review"]
    assert d.kind == "local-attestation"
    assert d.requires == "none"
    # The claude value carries the arg grammar with the <level> placeholder;
    # self_review_invocation substitutes a validated level (never ultra).
    # The scalar is the portable fallback an unknown harness receives.
    assert d.invocations == {
        "claude": "/code-review <level> --comment",
        "codex": "/review",
        "opencode": "/review-changes",
        "agy": "/fno:review",
    }
    assert d.invocation == "/fno:review"


def test_descriptors_are_frozen():
    with pytest.raises(Exception):
        _RESOLVABLE_REVIEWERS["declare"].asserts = "review-evidence"  # type: ignore[misc]


def test_validator_still_reads_keys(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Promoting the set to a mapping must be invisible to config load."""
    s = _settings(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers: [/sigma, declare]\n",
    )
    assert s.review.reviewers == ["sigma", "declare"]


def test_validator_rejection_message_lists_the_names(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The typo message must keep naming the allowed reviewers, not repr the
    descriptors - `sorted()` over a mapping yields keys, which is why the
    validator needed no edit."""
    with pytest.raises(Exception) as exc:
        _settings(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    reviewers: [teleport]\n",
        )
    assert "['code-review', 'declare', 'sigma']" in str(exc.value)


# --- `fno config doctor --review`, the diagnostic twin of the init refusal ---


def _doctor_review(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, reviewers: str, env: dict):
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(f"schema_version: 1\nconfig:\n  review:\n    reviewers: {reviewers}\n")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "CODEX_SESSION_ID",
                "GEMINI_SESSION_ID", "TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF"):
        monkeypatch.delenv(var, raising=False)
    for k, v in env.items():
        monkeypatch.setenv(k, v)
    load_settings.cache_clear()
    from fno.cli import app

    return CliRunner().invoke(app, ["config", "doctor", "--review"])


def test_doctor_review_reports_ok(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    r = _doctor_review(tmp_path, monkeypatch, "[sigma]", {"CLAUDE_CODE_SESSION_ID": "s1"})
    assert r.exit_code == 0
    assert "satisfiable: sigma" in r.output


def test_doctor_review_reports_unavailable(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    r = _doctor_review(
        tmp_path, monkeypatch, "[sigma]", {"GEMINI_SESSION_ID": "g1", "TARGET_UNATTENDED": "1"}
    )
    assert r.exit_code == 1
    assert "unavailable: sigma" in r.output
    assert "harness=gemini substrate=headless" in r.output
    assert "`declare` is never substituted for you" in r.output


def test_doctor_review_marks_declare_as_self_cert(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC5: the surface that prints `declare` says what it asserts."""
    r = _doctor_review(tmp_path, monkeypatch, "[declare]", {"CLAUDE_CODE_SESSION_ID": "s1"})
    assert r.exit_code == 0
    assert "asserts no review evidence" in r.output


def test_doctor_review_empty_gate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    r = _doctor_review(tmp_path, monkeypatch, "[]", {"CLAUDE_CODE_SESSION_ID": "s1"})
    assert r.exit_code == 0
    assert "no local reviewers gate" in r.output


def _doctor_peers(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    peers_toml: str,
    env: dict[str, str],
):
    cfg = tmp_path / "config.toml"
    cfg.write_text(f"[review]\npeers = {peers_toml}\n")
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    for var in (
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "TARGET_UNATTENDED",
        "FNO_BG",
        "FNO_AGENT_SELF",
    ):
        monkeypatch.delenv(var, raising=False)
    for key, value in env.items():
        monkeypatch.setenv(key, value)
    load_settings.cache_clear()
    from fno.cli import app

    return CliRunner().invoke(app, ["config", "doctor", "--review"])


def test_doctor_review_reports_identity_free_cross_model_peer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    r = _doctor_peers(
        tmp_path,
        monkeypatch,
        '["codex", {provider = "claude", model = "zai,glm-5.2"}]',
        {"CODEX_THREAD_ID": "c1"},
    )
    assert r.exit_code == 0, r.output
    assert "local peer gate: satisfiable" in r.output
    assert "claude via zai,glm-5.2" in r.output
    assert "codex" in r.output and "same model" in r.output


def test_doctor_review_refuses_only_same_model_local_peer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    r = _doctor_peers(
        tmp_path,
        monkeypatch,
        '["codex"]',
        {"CODEX_THREAD_ID": "c1"},
    )
    assert r.exit_code == 1
    assert "local peer gate: unavailable" in r.output
    assert "same model" in r.output


def test_doctor_does_not_treat_a_codex_model_field_as_a_route(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    r = _doctor_peers(
        tmp_path,
        monkeypatch,
        '[{provider = "codex", model = "zai,glm-5.2"}]',
        {"CODEX_THREAD_ID": "c1"},
    )
    assert r.exit_code == 1
    assert "same model" in r.output


# --- the refusal message's two hints (AC10-ERR / AC11-ERR / AC12-HP) ---


def test_a_near_miss_names_the_reviewer_it_resembles(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC10-ERR."""
    with pytest.raises(Exception) as exc:
        _settings(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    reviewers: [sgima]\n",
        )
    assert "Did you mean 'sigma'?" in str(exc.value)


def test_a_wrong_key_value_points_at_the_adjacent_key(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC11-ERR: `coderabbit` belongs to external_reviewers, not this gate."""
    with pytest.raises(Exception) as exc:
        _settings(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    reviewers: [coderabbit]\n",
        )
    assert "config.review.external_reviewers" in str(exc.value)


def test_the_wrong_key_check_beats_difflib(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """`codex` is a VALID external_reviewers value, so a fuzzy match would send
    the operator to fix the wrong line."""
    with pytest.raises(Exception) as exc:
        _settings(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    reviewers: [codex]\n",
        )
    message = str(exc.value)
    assert "config.review.external_reviewers" in message
    assert "Did you mean" not in message


def test_an_unrecognizable_name_gets_no_invented_guess(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    with pytest.raises(Exception) as exc:
        _settings(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    reviewers: [teleport]\n",
        )
    message = str(exc.value)
    assert "Did you mean" not in message
    assert "external_reviewers" not in message


def test_the_valid_set_is_unchanged(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """AC12-HP."""
    s = _settings(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n"
        "    reviewers: [sigma, /code-review, declare]\n",
    )
    assert s.review.reviewers == ["sigma", "code-review", "declare"]


# --- config.review.reviewer_registry (AC9-INV) ---


def _toml_settings(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, body: str):
    """Load a FLAT config.toml - the real file shape (no `config:` wrapper)."""
    cfg = tmp_path / "config.toml"
    cfg.write_text(body)
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()
    return load_settings()


_REGISTRY_TOML = """\
[review]
reviewers = ["/my-security-skill"]

[review.reviewer_registry.my-security-skill]
kind = "harness-skill"
requires = "skill"
invocation = "/my-security-skill"
asserts = "invocation"
"""


def test_a_registered_reviewer_resolves(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    s = _toml_settings(tmp_path, monkeypatch, _REGISTRY_TOML)
    assert s.review.reviewers == ["my-security-skill"]
    d = s.review.reviewer_registry["my-security-skill"]
    assert (d.kind, d.requires, d.asserts) == ("harness-skill", "skill", "invocation")


def test_a_registry_entry_never_enters_the_builtin_table(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC9-INV: the constant the parity script AST-parses stays footnote's own,
    so its verdict cannot vary with the local machine's config."""
    s = _toml_settings(tmp_path, monkeypatch, _REGISTRY_TOML)
    assert set(_RESOLVABLE_REVIEWERS) == {"sigma", "code-review", "declare"}
    union = resolvable_reviewers(s.review.reviewer_registry)
    assert "my-security-skill" in union
    assert "my-security-skill" not in _RESOLVABLE_REVIEWERS


def test_a_registry_entry_cannot_shadow_a_builtin(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """A project redefining `sigma` as a self-cert would weaken a shipped gate."""
    s = _toml_settings(
        tmp_path,
        monkeypatch,
        '[review.reviewer_registry.sigma]\nkind = "harness-skill"\n'
        'requires = "none"\ninvocation = "/nope"\nasserts = "self-cert"\n',
    )
    assert resolvable_reviewers(s.review.reviewer_registry)["sigma"] is (
        _RESOLVABLE_REVIEWERS["sigma"]
    )


def test_an_unregistered_name_still_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    with pytest.raises(Exception) as exc:
        _toml_settings(
            tmp_path,
            monkeypatch,
            '[review]\nreviewers = ["/other-skill"]\n\n'
            '[review.reviewer_registry.my-skill]\nkind = "harness-skill"\n'
            'requires = "skill"\ninvocation = "/my-skill"\nasserts = "invocation"\n',
        )
    assert "unresolvable reviewer" in str(exc.value)


def test_a_project_probe_list_is_readable_python_side(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The Rust gate enforces this key; Python must not be blind to it. Flat
    root, not nested under a `config` table."""
    s = _toml_settings(
        tmp_path, monkeypatch, 'done_probes = ["make a11y-check", "semgrep --error"]\n'
    )
    assert s.done_probes == ["make a11y-check", "semgrep --error"]
    assert _toml_settings(tmp_path, monkeypatch, "plans_dir = 'x'\n").done_probes == []


def test_a_wrong_typed_probe_list_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Mirrors the Rust side's Unparseable: a guardrail that disappears when you
    typo it is not a guardrail."""
    with pytest.raises(Exception):
        _toml_settings(tmp_path, monkeypatch, '[done_probes]\na = "b"\n')


def test_doctor_reports_the_resolved_gates(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC14-UI: an operator learns their guardrail is only WITNESSED here,
    rather than at the stop gate after the work is done."""
    cfg = tmp_path / "config.toml"
    cfg.write_text('done_probes = ["make a11y-check"]\n\n' + _REGISTRY_TOML)
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "s1")
    load_settings.cache_clear()
    from fno.cli import app

    r = CliRunner().invoke(app, ["config", "doctor"])
    assert "probe (project): make a11y-check" in r.output
    assert "reviewer: my-security-skill - asserts invocation" in r.output
    assert "no claim about its verdict" in r.output


def test_a_registry_entry_with_a_bad_rung_is_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    with pytest.raises(Exception):
        _toml_settings(
            tmp_path,
            monkeypatch,
            '[review.reviewer_registry.my-skill]\nkind = "harness-skill"\n'
            'requires = "skill"\ninvocation = "/my-skill"\nasserts = "trust-me"\n',
        )

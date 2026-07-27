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

from fno.config import _RESOLVABLE_REVIEWERS, ReviewerDescriptor, load_settings


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
        assert d.kind in {"local-attestation", "github-app", "external-cli", "human"}
        assert d.requires in {"none", "subagent-dispatch", "operator"}
        assert d.asserts in {"review-evidence", "self-cert"}
        assert d.invocation.strip(), f"{name} must name how to satisfy the gate"


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


def test_code_review_is_operator_driven():
    """`/code-review` is user-triggered and billed; no agent can launch it."""
    d = _RESOLVABLE_REVIEWERS["code-review"]
    assert d.kind == "human"
    assert d.requires == "operator"


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
        tmp_path, monkeypatch, "[sigma]", {"CODEX_THREAD_ID": "t1", "TARGET_UNATTENDED": "1"}
    )
    assert r.exit_code == 1
    assert "unavailable: sigma" in r.output
    assert "harness=codex substrate=headless" in r.output
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

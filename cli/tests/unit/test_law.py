"""The law door is native on the fno Rust front; Python keeps the validator.

`fno inbox law set` has no Python surface: the front classifies the verb
before the CLI runs. The door's gate tests run in Rust
(`crates/fno-agents/src/law_match.rs`, the record-door section); what stays
here is the Python library gate `record_decision` still enforces for its
other callers, and the fail-closed validator round-trip.

Every refusal here asserts an EXACT exit code. `typer` already spends exit 2 on
usage errors, so a bare non-zero assertion proves the command failed and nothing
about WHY. Each refusal also carries a make-it-fail probe: the same call with
the one refused input removed records a `d-` id, which is what proves the gate
is the thing refusing.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from tests.unit._front_dev import front_dev_binary, worker_dev_binary

pytestmark = pytest.mark.skipif(
    front_dev_binary() is None,
    reason="compiled fno front binary not present (build with `cargo build --manifest-path crates/fno/Cargo.toml --bin fno)`",
)


@pytest.fixture(autouse=True)
def _point_the_front_at_the_dev_worker(monkeypatch: pytest.MonkeyPatch) -> None:
    """The front serves the law door by spawning the runtime worker; a dev
    checkout's front must never answer from an installed worker."""
    worker = worker_dev_binary()
    if worker is not None:
        monkeypatch.setenv("FNO_AGENTS_WORKER", str(worker))


def _rows(index):
    from tests._event_rows import event_rows

    return event_rows(index)


def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Hermetic state for both languages: the env the Rust door reads and
    the monkeypatch the Python library still takes."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    monkeypatch.setenv("FNO_HOME", str(tmp_path / "state"))
    from fno import paths

    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    index = tmp_path / "state" / "decisions.jsonl"
    index.parent.mkdir(exist_ok=True)
    index.touch()
    import fno.decide

    monkeypatch.setattr(fno.decide, "_decisions_index_path", lambda: index)
    # The law door stamps the recording project and refuses an unmapped one,
    # so the fixture provisiones a hermetic work map naming the pytest cwd
    # itself (a direct match, layout-independent).
    map_file = tmp_path / "settings.yaml"
    map_file.write_text(
        "work:\n"
        "  workspaces:\n"
        "    main:\n"
        "      projects:\n"
        "        - name: fno\n"
        f"          path: {Path.cwd()}\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(map_file))
    return index


def _as_chat_session(monkeypatch: pytest.MonkeyPatch) -> None:
    """Make the PYTHON resolver see a session someone typed into. The Rust
    gate reads process ancestry and cannot be faked from here; the library
    tests below use this for the legs that stayed Python."""
    from types import SimpleNamespace

    from fno.agents import self_stamp

    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id="a" * 32, harness="claude"),
    )


# ── no Python law surface remains ─────────────────────────────────────────────


def test_no_python_law_surface_remains() -> None:
    """The front owns the verbs; the proposal-era names stay gone."""
    from fno import law

    assert not hasattr(law, "law_app")
    for retired in (
        "prepare_proposal",
        "enact_proposal",
        "load_proposal",
        "proposal_lock",
        "validate_operator_consent",
    ):
        assert not hasattr(law, retired), retired

    from fno import paths

    assert not hasattr(paths, "law_proposals_dir")


def test_the_root_loader_has_no_law_entry() -> None:
    """The deprecated root `fno law` mount is gone with the group."""
    from fno.cli import LAZY_SUBCOMMANDS

    assert "law" not in LAZY_SUBCOMMANDS


def test_library_refuses_chat_attested_from_an_unmarked_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The gate holds below the CLI too.

    `record_decision` is importable, so a resolver enforced only in the command
    body would be a gate anything using the library walks around.
    """
    from fno.decide import UnattributedAuthorityError, record_decision

    index = _isolate(tmp_path, monkeypatch)
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)

    def no_identity(*a, **k):
        from types import SimpleNamespace

        return SimpleNamespace(session_id=None, harness=None)

    from fno.agents import self_stamp

    monkeypatch.setattr(self_stamp, "resolve_self_identity", no_identity)

    with pytest.raises(UnattributedAuthorityError):
        record_decision(
            subject="merge-authority",
            decision="Merges belong to the operator",
            rationale="why",
            authority_source="chat_attested",
        )
    assert _rows(index) == []


def test_library_refuses_a_coordination_statement_in_the_law_lane(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The statement classifier holds below the CLI, like the session gate."""
    from fno.decide import record_decision
    from fno.law import LawValidationError

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    with pytest.raises(LawValidationError, match="coordination"):
        record_decision(
            subject="merge-authority",
            decision="This PR merges without review",
            rationale="why",
            authority_source="chat_attested",
        )
    assert _rows(index) == []


def test_an_unavailable_validator_refuses_the_recording(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fail closed: a statement nobody could classify records nothing. The
    library wrapper refuses on an unavailable front."""
    from fno.rust_binary import VerbUnavailable

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    def down(*a, **k):
        raise VerbUnavailable("the native fno binary was not found")

    monkeypatch.setattr("fno.rust_binary.call_front_json", down)
    from fno.law import LawValidationError, validate_durable_law

    with pytest.raises(LawValidationError, match="law validation is unavailable"):
        validate_durable_law(
            subject="review-rounds",
            decision="Two rounds.",
            rationale="r",
        )
    assert _rows(index) == []


def test_validate_durable_law_refuses_a_coordination_statement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)

    from fno.law import LawValidationError, validate_durable_law

    with pytest.raises(LawValidationError, match="coordination"):
        validate_durable_law(
            subject="merge-authority",
            decision="This PR merges without review",
            rationale="why",
        )
    assert _rows(index) == []

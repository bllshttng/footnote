"""The law door lives in the crate; the Python command is a shim.

`fno inbox law set` forwards its arguments to `fno-agents law-match record`,
which owns every gate and flag natively (--global, --paths included). The
door's gate tests run in Rust (`crates/fno-agents/src/law_match.rs`, the
record-door section); what stays here is the shim contract and the Python
library gates that `record_decision` still enforces for its other callers.

Every refusal here asserts an EXACT exit code. `typer` already spends exit 2 on
usage errors, so a bare non-zero assertion proves the command failed and nothing
about WHY. Each refusal also carries a make-it-fail probe: the same call with
the one refused input removed records a `d-` id, which is what proves the gate
is the thing refusing.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.rust_binary import find_dev_binary

LAW_REFUSED_EXIT = 3

pytestmark = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents)`",
)


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


def _run(args: list[str]):
    """Invoke through a mounted parent, the way `fno inbox law` reaches it."""
    import typer

    from fno.law import law_app

    parent = typer.Typer()
    parent.add_typer(law_app, name="law")
    return CliRunner().invoke(parent, ["law", *args])


# ── the shim contract: forward argv, mirror the exit code ─────────────────────


def test_shim_forwards_argv_to_the_record_door(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The shim's whole job: every argument, flag included, rides to
    `law-match record`, and the door's exit code is the command's."""
    _isolate(tmp_path, monkeypatch)
    from types import SimpleNamespace

    seen: dict = {}

    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", lambda: Path("/stub/fno-agents"))
    monkeypatch.setattr(
        "subprocess.run",
        lambda args, **k: seen.update(args=args) or SimpleNamespace(returncode=0),
    )

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to the operator",
            "--rationale",
            "why",
            "--global",
            "--paths",
            "crates/**",
        ]
    )

    assert result.exit_code == 0, result.output
    argv = seen["args"]
    assert argv[1:3] == ["law-match", "record"]
    assert "merge-authority" in argv
    assert "Merges belong to the operator" in argv
    assert "--global" in argv
    assert argv[argv.index("--paths") + 1] == "crates/**"
    assert "--rationale" in argv


def test_shim_mirrors_the_door_exit_code(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Exit 1 (recorded-but-index-failed) and exit 3 (refused) pass through:
    the caller reads the door's answer, not a reinterpreted one."""
    _isolate(tmp_path, monkeypatch)
    from types import SimpleNamespace

    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", lambda: Path("/stub/fno-agents"))
    for door_exit in (1, 3, 2):
        monkeypatch.setattr(
            "subprocess.run", lambda *a, e=door_exit, **k: SimpleNamespace(returncode=e)
        )
        result = _run(["set", "topic", "The body", "--rationale", "why"])
        assert result.exit_code == door_exit, (door_exit, result.output)


def test_shim_refuses_when_the_binary_is_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", lambda: None)

    result = _run(["set", "topic", "The body"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "binary is unavailable" in result.output
    assert _rows(tmp_path / "state" / "decisions.jsonl") == []


# ── the statement is not durable law: the validator still refuses ─────────────


def test_no_staged_proposal_surface_remains() -> None:
    """prepare / enact / resume / inspect are gone, hash and receipt with them."""
    from fno import law
    from fno.law import law_app

    commands = {command.name for command in law_app.registered_commands}
    assert commands == {"set"}
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
    shim's exit contract mirrors the door, and the library wrapper refuses on
    an unavailable verb."""
    from fno.rust_binary import VerbUnavailable

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    def down(*a, **k):
        raise VerbUnavailable("the fno-agents binary was not found")

    monkeypatch.setattr("fno.rust_binary.verb_call", down)
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

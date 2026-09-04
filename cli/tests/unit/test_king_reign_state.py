"""``reign_state``: the Python client over the Rust reign reader.

The reader's semantics (agreeing, split, no manifest, unreadable registry,
terminal rows, unsafe scopes, the shape rewrite) live in
``crates/fno-agents/src/loop_reign.rs`` and are pinned by that module's own
tests. What these tests pin is the client half: the JSON maps onto the
dataclass unchanged, a missing or failing binary answers unknown with a named
reason (never a clean ``False``), and the argv carries the caller's intent.
"""
from __future__ import annotations

import json
import stat
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir

PAYLOAD = {
    "crowned": True,
    "scope": "alpha",
    "shape": "pass",
    "manifest_session": "aaaa1111-0000-4000-8000-000000000001",
    "registry_session": "aaaa1111-0000-4000-8000-000000000001",
    "live": True,
    "split": False,
    "unknown_reason": None,
}


def _stub_binary(tmp_path: Path, body: str, *, name: str = "fno-agents") -> Path:
    path = tmp_path / name
    path.write_text("#!/usr/bin/env python3\n" + body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    return path


def test_ac1_manifest_written_by_init_carries_shape_pass(tmp_path: Path) -> None:
    from fno.king.state import king_manifest_path, parse_manifest, write_manifest

    path = king_manifest_path("alpha", state_root=tmp_path / ".fno")
    write_manifest(
        path,
        scope="alpha",
        harness_session_id="aaaa1111-0000-4000-8000-000000000001",
        owner_cwd=str(tmp_path),
    )
    fields = parse_manifest(path)
    assert fields["shape"] == "pass"
    # A court-shaped write records court verbatim, not normalized away.
    court = king_manifest_path("beta", state_root=tmp_path / ".fno")
    write_manifest(
        court,
        scope="beta",
        harness_session_id="aaaa2222-0000-4000-8000-000000000002",
        shape="court",
        owner_cwd=str(tmp_path),
    )
    assert parse_manifest(court)["shape"] == "court"


def test_client_maps_the_rust_payload_onto_the_dataclass(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import reign_state

    use_tmpdir(monkeypatch, tmp_path)
    script = _stub_binary(
        tmp_path, "import sys\nsys.stdout.write(" + repr(json.dumps(PAYLOAD)) + ")\n"
    )
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: script)

    state = reign_state(scope="alpha")

    assert state.crowned is True
    assert state.scope == "alpha"
    assert state.shape == "pass"
    assert state.live is True
    assert state.split is False
    assert state.unknown_reason is None


def test_client_argv_carries_scope_session_and_registry(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import reign_state

    use_tmpdir(monkeypatch, tmp_path)
    log = tmp_path / "argv.json"
    script = _stub_binary(
        tmp_path,
        "import json, sys\n"
        f"json.dump(sys.argv[1:], open({str(log)!r}, 'w'))\n"
        "sys.stdout.write(" + repr(json.dumps(PAYLOAD)) + ")\n",
    )
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: script)
    monkeypatch.chdir(tmp_path)

    reign_state(scope="alpha", session_id="aaaa1111-0000-4000-8000-000000000001")

    argv = json.loads(log.read_text())
    assert argv[0] == "reign-state"
    assert "--scope" in argv and argv[argv.index("--scope") + 1] == "alpha"
    assert "--session" in argv
    assert (
        argv[argv.index("--session") + 1] == "aaaa1111-0000-4000-8000-000000000001"
    )
    assert "--registry" in argv
    assert "--root" in argv


def test_missing_binary_answers_unknown_never_clean_false(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import reign_state

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)
    state = reign_state(scope="alpha")
    assert state.live is None
    assert state.crowned is None
    assert state.split is None
    assert state.unknown_reason is not None
    assert "binary" in state.unknown_reason


def test_failing_binary_carries_its_stderr_as_the_reason(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.king.state import reign_state

    use_tmpdir(monkeypatch, tmp_path)
    script = _stub_binary(
        tmp_path,
        "import sys\nsys.stderr.write('unsafe king scope')\nsys.exit(1)\n",
    )
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: script)
    state = reign_state(scope="alpha")
    assert state.crowned is None
    assert "unsafe king scope" in (state.unknown_reason or "")
    assert "exited 1" in (state.unknown_reason or "")


def test_garbage_stdout_is_unknown_not_a_crash(tmp_path: Path, monkeypatch) -> None:
    from fno.king.state import reign_state

    use_tmpdir(monkeypatch, tmp_path)
    script = _stub_binary(tmp_path, "print('not json')\n")
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: script)
    state = reign_state(scope="alpha")
    assert state.crowned is None
    assert "no JSON" in (state.unknown_reason or "")


def test_rust_reader_semantics_live_in_the_crate() -> None:
    """The semantic cases (split, terminal, unsafe scope, legacy shape) are
    pinned by loop_reign.rs's own tests; this file pins only the client."""
    crate = Path(__file__).parents[3] / "crates" / "fno-agents" / "src" / "loop_reign.rs"
    assert crate.is_file(), f"the Rust reader moved: {crate}"
    text = crate.read_text(encoding="utf-8")
    for needle in (
        "sessions_differ_is_split",
        "unknown_never_clean_false",
        "unsafe_scope_with_unreadable_registry",
        "terminal_crown_row_is_not_a_live_reign",
        "legacy_manifest_without_shape_reads_as_pass",
        "never_falls_through_to_a_claude_prefix",
    ):
        assert needle in text, f"the Rust suite lost the {needle!r} case"

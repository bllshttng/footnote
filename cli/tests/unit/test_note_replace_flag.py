"""The note bridge carries --replace to the native action: the cross-session
guard's explicit door must survive the subprocess hop on both routes."""

from types import SimpleNamespace

# Import cli first: note_cli participates in a circular import with it, and
# importing note_cli alone trips the mid-import verb classification.
from fno.graph import cli as graph_cli  # noqa: F401
from fno.graph import note_cli


def test_replace_flag_reaches_the_native_argv(monkeypatch, tmp_path):
    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        return SimpleNamespace(returncode=0, stdout='{"line": "noted"}', stderr="")

    monkeypatch.setattr(note_cli.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: tmp_path / "fno-agents")
    code, receipt = note_cli._write_state(
        "x-1",
        "body",
        quiet=True,
        session_id="s1",
        graph_path=tmp_path / "graph.json",
        replace=True,
    )
    assert code == 0
    assert receipt == {"line": "noted"}
    assert seen["argv"][-1] == "--replace"

    note_cli._write_state(
        "x-1", "body", quiet=True, session_id="s1", graph_path=tmp_path / "graph.json"
    )
    assert "--replace" not in seen["argv"]

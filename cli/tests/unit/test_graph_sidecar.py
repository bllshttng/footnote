"""load_graph's SHA256 sidecar contract, after the port.

The two-write window the old retry/wait machinery raced against is gone
structurally: the keeper publishes graph.json and its sidecar as two atomic
replaces under one bounded lock, and ``read_file_bytes`` is served under
that same gate, so a reader can never observe new bytes against an old
sidecar. What remains is the contract itself: first contact blesses, a
match reads, and a mismatch is corruption, raised immediately.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from fno.graph.load import GraphCorruptionError, _is_sha256, load_graph


def _write(tmp_path: Path, entries: list[dict]) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}) + "\n")
    return p


def test_absent_sidecar_is_blessed_on_first_contact(tmp_path):
    g = _write(tmp_path, [{"id": "ab-1st00001", "title": "first"}])
    rows = load_graph(g)
    assert [e["id"] for e in rows] == ["ab-1st00001"]
    sidecar = Path(str(g) + ".sha256")
    assert _is_sha256(sidecar.read_text().strip())


def test_matching_sidecar_reads_entries(tmp_path):
    g = _write(tmp_path, [{"id": "ab-match001", "title": "ok"}])
    load_graph(g)  # bless
    rows = load_graph(g)  # validated read
    assert [e["id"] for e in rows] == ["ab-match001"]


def test_mismatch_raises_immediately(tmp_path):
    g = _write(tmp_path, [{"id": "ab-corrupt1", "title": "doomed"}])
    load_graph(g)  # bless
    g.write_text(json.dumps({"entries": [{"id": "ab-other0001", "title": "edited"}]}) + "\n")
    with pytest.raises(GraphCorruptionError):
        load_graph(g)


def test_garbage_sidecar_is_reblessed_with_a_warning(tmp_path, capsys):
    g = _write(tmp_path, [{"id": "ab-garbage1", "title": "x"}])
    Path(str(g) + ".sha256").write_text("not-a-digest")
    rows = load_graph(g)
    assert [e["id"] for e in rows] == ["ab-garbage1"]
    assert "corruption detection was disabled" in capsys.readouterr().err


def test_the_sidecar_tracks_the_kept_bytes(tmp_path):
    g = _write(tmp_path, [{"id": "ab-track001", "title": "x"}])
    load_graph(g)
    sidecar = Path(str(g) + ".sha256")
    assert hashlib.sha256(g.read_bytes()).hexdigest() == sidecar.read_text().strip()

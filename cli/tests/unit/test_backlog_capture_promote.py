"""Tests for promote / dismiss / archive (Wave 1.3).

Covers AC4-ERR, AC4-EDGE, AC4-FR, AC6-HP.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

runner = CliRunner()


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    # chdir into tmp_path: a prior integration test may leave the process cwd
    # pointing at a deleted tmp dir, which makes os.getcwd() raise and silently
    # routes graph writes to the real ~/.fno. A valid cwd prevents that.
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    yield
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()


def _seed_item(tmp_path: Path, **kw) -> dict:
    from fno.backlog.capture import add_item
    inbox = tmp_path / "inbox.md"
    return add_item(
        inbox,
        title=kw.get("title", "promote me"),
        source=kw.get("source", "PR#1"),
        why=kw.get("why", "w"),
        where=kw.get("where", "x"),
        priority=kw.get("priority", "p2"),
    )


# --------------------------------------------------------------------------
# promote
# --------------------------------------------------------------------------

def test_promote_creates_node_and_strikes_checkbox(tmp_path: Path) -> None:
    """AC4-FR: node created, then checkbox struck with -> ab-id."""
    from fno.backlog.capture import promote_item
    item = _seed_item(tmp_path)
    inbox = tmp_path / "inbox.md"
    # Explicit graph path: keeps the test deterministic regardless of global
    # path-resolution state (an upstream test can leave the process cwd on a
    # deleted dir, which makes the _constants helpers fail-open to ~/.fno).
    gp = tmp_path / "graph.json"

    res = promote_item(inbox, item["id"], graph_path=gp)
    assert res["id"] == item["id"]
    assert res["node_id"].startswith("ab-")
    assert res["status"] == "promoted"

    text = inbox.read_text(encoding="utf-8")
    assert f"- [x] {item['id']}" in text
    assert f"-> {res['node_id']}" in text

    # The graph node really exists in the graph promote wrote to.
    from fno.graph.store import read_graph
    nodes = read_graph(gp)
    assert any(n["id"] == res["node_id"] for n in nodes)


def test_promote_rejects_invalid_priority_override(tmp_path: Path) -> None:
    """Codex P2: a --priority override outside p0-p3 must be rejected, not written to graph.json."""
    from fno.backlog.capture import promote_item, InboxValidationError
    item = _seed_item(tmp_path)
    inbox = tmp_path / "inbox.md"
    gp = tmp_path / "graph.json"
    with pytest.raises(InboxValidationError):
        promote_item(inbox, item["id"], priority="p9", graph_path=gp)
    # No node created, item still open.
    assert not gp.exists() or "p9" not in gp.read_text(encoding="utf-8")
    assert f"- [ ] {item['id']}" in inbox.read_text(encoding="utf-8")


def test_promote_unknown_id_raises(tmp_path: Path) -> None:
    """AC4-ERR: unknown fu-id is rejected."""
    from fno.backlog.capture import promote_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    inbox.write_text("# empty\n", encoding="utf-8")
    with pytest.raises(InboxValidationError):
        promote_item(inbox, "fu-zzzzzz")


def test_promote_is_idempotent(tmp_path: Path) -> None:
    """AC4-EDGE: re-promoting reports the existing node, creates no duplicate."""
    from fno.backlog.capture import promote_item
    from fno.graph.store import read_graph
    item = _seed_item(tmp_path)
    inbox = tmp_path / "inbox.md"
    gp = tmp_path / "graph.json"

    first = promote_item(inbox, item["id"], graph_path=gp)
    count_after_first = len(read_graph(gp))

    second = promote_item(inbox, item["id"], graph_path=gp)
    assert second["node_id"] == first["node_id"]
    assert second["status"] == "already_promoted"
    assert len(read_graph(gp)) == count_after_first  # no duplicate node


# --------------------------------------------------------------------------
# dismiss
# --------------------------------------------------------------------------

def test_dismiss_strikes_with_reason(tmp_path: Path) -> None:
    from fno.backlog.capture import dismiss_item, parse_items
    item = _seed_item(tmp_path)
    inbox = tmp_path / "inbox.md"

    res = dismiss_item(inbox, item["id"], reason="out of scope")
    assert res["status"] == "dismissed"
    text = inbox.read_text(encoding="utf-8")
    assert f"- [-] {item['id']}" in text
    assert "dismissed: out of scope" in text
    # No longer an open item
    assert all(i["id"] != item["id"] for i in parse_items(text))


def test_dismiss_unknown_id_raises(tmp_path: Path) -> None:
    from fno.backlog.capture import dismiss_item, InboxValidationError
    inbox = tmp_path / "inbox.md"
    inbox.write_text("# empty\n", encoding="utf-8")
    with pytest.raises(InboxValidationError):
        dismiss_item(inbox, "fu-zzzzzz", reason="x")


# --------------------------------------------------------------------------
# archive
# --------------------------------------------------------------------------

def test_archive_sweeps_struck_leaves_open(tmp_path: Path) -> None:
    """AC6-HP: archive moves struck items to sibling file, leaves open ones."""
    from fno.backlog.capture import (
        add_item,
        archive_struck,
        dismiss_item,
        parse_items,
    )
    inbox = tmp_path / "inbox.md"
    keep = add_item(inbox, title="keep", source="PR#1", why="w", where="x")
    drop = add_item(inbox, title="drop", source="PR#2", why="w", where="x")
    dismiss_item(inbox, drop["id"], reason="nope")

    res = archive_struck(inbox)
    assert res["archived"] == 1

    text = inbox.read_text(encoding="utf-8")
    open_items = parse_items(text)
    assert [i["id"] for i in open_items] == [keep["id"]]
    assert drop["id"] not in text  # moved out

    archive_path = Path(res["archive_path"])
    assert archive_path.exists()
    assert drop["id"] in archive_path.read_text(encoding="utf-8")


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def test_cli_promote_unknown_exits_nonzero(tmp_path: Path) -> None:
    from fno.backlog.capture import cli
    inbox = tmp_path / "internal/fno/backlog/inbox.md"
    inbox.parent.mkdir(parents=True, exist_ok=True)
    inbox.write_text("# empty\n", encoding="utf-8")
    res = runner.invoke(cli, ["promote", "fu-zzzzzz"])
    assert res.exit_code != 0


def test_cli_promote_emits_event(tmp_path: Path) -> None:
    from fno.backlog.capture import cli
    add = runner.invoke(
        cli, ["add", "promote via cli", "--source", "PR#1", "--why", "w", "--where", "x"]
    )
    fu = json.loads(add.stdout)["id"]
    res = runner.invoke(cli, ["promote", fu])
    assert res.exit_code == 0, res.output
    events = (tmp_path / ".fno" / "events.jsonl").read_text().splitlines()
    types = [json.loads(l)["type"] for l in events if l.strip()]
    assert "capture_promote" in types

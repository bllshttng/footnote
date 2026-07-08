"""Tests for `fno plan migrate-folder` (fno.plan._migrate)."""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.plan._migrate import MigrateError, migrate_folder


EXEC_STRATEGY = """\
## Execution Strategy

```yaml
execution_mode: mixed
waves:
- wave: 1
  mode: sequential
  tasks:
  - '1.1'
```
"""


def _folder(tmp_path: Path, *, phases=(), completion=None, status="done", prs=(3,)) -> Path:
    d = tmp_path / "2026-05-20-example"
    d.mkdir()
    pr_lines = "".join(f"  - {p}\n" for p in prs)
    (d / "00-INDEX.md").write_text(
        f"---\nstatus: {status}\nshipped_at: '2026-05-20T10:00'\nurls: [https://x/1]\n"
        f"session_ids: [abc]\nprs:\n{pr_lines}---\n\n# Example\n\n{EXEC_STRATEGY}",
        encoding="utf-8",
    )
    for i, body in enumerate(phases, start=1):
        (d / f"{i:02d}-phase.md").write_text(
            f"---\nphase: {i}\ntitle: t{i}\n---\n\n# Phase {i}\n\n{body}\n",
            encoding="utf-8",
        )
    if completion is not None:
        (d / "COMPLETION.md").write_text(completion, encoding="utf-8")
    return d


def test_relocation_preserves_frontmatter_and_exec_strategy_verbatim(tmp_path):
    d = _folder(tmp_path, phases=("first phase body", "second phase body"))
    orig_index = (d / "00-INDEX.md").read_text(encoding="utf-8")

    res = migrate_folder(d)

    assert not res.skipped
    assert res.phase_count == 2
    doc = res.new_doc_path.read_text(encoding="utf-8")
    # Frontmatter byte-preserved (index text is the head of the new doc).
    assert doc.startswith(orig_index.rstrip("\n"))
    # Execution Strategy YAML block rides along verbatim.
    assert EXEC_STRATEGY.strip() in doc
    # Phase bodies inlined in order, frontmatter stripped.
    assert doc.index("first phase body") < doc.index("second phase body")
    assert "phase: 1" not in doc  # phase frontmatter stripped
    # Folder archived beside the new doc; original gone.
    assert res.archived_dir.is_dir()
    assert not d.exists()
    assert res.new_doc_path.name == "2026-05-20-example.md"


def test_lettered_phase_files_are_not_dropped(tmp_path):
    # The vault uses letter-suffixed phases (02b-, 04c-); dropping one would
    # silently lose a phase body. All must inline, in order.
    d = tmp_path / "2026-05-20-example"
    d.mkdir()
    (d / "00-INDEX.md").write_text("---\nstatus: done\n---\n\n# Head\n", encoding="utf-8")
    (d / "01-a.md").write_text("# P1\n\nalpha\n", encoding="utf-8")
    (d / "01b-a.md").write_text("# P1b\n\nbeta\n", encoding="utf-8")
    (d / "02-a.md").write_text("# P2\n\ngamma\n", encoding="utf-8")

    res = migrate_folder(d)
    doc = res.new_doc_path.read_text(encoding="utf-8")
    assert res.phase_count == 3
    assert doc.index("alpha") < doc.index("beta") < doc.index("gamma")


def test_read_failure_surfaces_clean_error(tmp_path):
    d = _folder(tmp_path)
    # A phase file that is not valid UTF-8 -> clean MigrateError, folder intact.
    (d / "01-bad.md").write_bytes(b"\xff\xfe not utf8")
    with pytest.raises(MigrateError) as exc:
        migrate_folder(d)
    assert exc.value.kind == "read-failed"
    assert d.is_dir()
    assert not (tmp_path / "2026-05-20-example.md").exists()


def test_completion_folds_in_as_section(tmp_path):
    d = _folder(tmp_path, phases=("body",), completion="Shipped in PR #3. All green.")
    res = migrate_folder(d)
    doc = res.new_doc_path.read_text(encoding="utf-8")
    assert res.folded_completion
    assert "## Completion Log" in doc
    assert "Shipped in PR #3. All green." in doc


def test_index_only_folder_produces_valid_doc(tmp_path):
    d = _folder(tmp_path)  # no phases, no completion
    res = migrate_folder(d)
    doc = res.new_doc_path.read_text(encoding="utf-8")
    assert res.phase_count == 0
    assert not res.folded_completion
    assert "# Example" in doc


def test_idempotent_when_target_exists(tmp_path):
    d = _folder(tmp_path)
    migrate_folder(d)  # first migration archives d, writes the .md
    # Recreate the folder to prove the sibling .md, not folder absence, gates.
    d.mkdir()
    (d / "00-INDEX.md").write_text("---\nstatus: done\n---\n\n# Again\n", encoding="utf-8")
    res = migrate_folder(d)
    assert res.skipped
    assert "already migrated" in res.message


def test_archived_input_is_noop(tmp_path):
    d = tmp_path / "plan-archived"
    d.mkdir()
    res = migrate_folder(d)
    assert res.skipped
    assert "already archived" in res.message


def test_missing_index_raises_not_found(tmp_path):
    d = tmp_path / "bare"
    d.mkdir()
    with pytest.raises(MigrateError) as exc:
        migrate_folder(d)
    assert exc.value.kind == "not-found"


def test_failed_migration_leaves_folder_intact(tmp_path):
    d = _folder(tmp_path, phases=("body",))
    # Pre-create the archive target so the folder rename fails after the doc
    # lands; the verb must roll back the doc and leave the folder untouched.
    (tmp_path / "2026-05-20-example-archived").mkdir()
    with pytest.raises(MigrateError) as exc:
        migrate_folder(d)
    assert exc.value.kind == "collision"
    assert d.is_dir()
    assert (d / "00-INDEX.md").exists()
    assert not (tmp_path / "2026-05-20-example.md").exists()  # no partial doc


def test_update_node_refuses_dispatch_armed(tmp_path, monkeypatch):
    d = _folder(tmp_path, status="ready")
    monkeypatch.setattr("fno.plan._migrate._node_status", lambda nid: "ready")
    called = {"set": False}
    monkeypatch.setattr(
        "fno.plan._migrate._set_node_plan_path",
        lambda *a, **k: called.__setitem__("set", True),
    )
    with pytest.raises(MigrateError) as exc:
        migrate_folder(d, update_node="ab-deadbeef")
    assert exc.value.kind == "dispatch-armed"
    # Neither the folder nor plan_path changed.
    assert d.is_dir()
    assert not (tmp_path / "2026-05-20-example.md").exists()
    assert not called["set"]


def test_update_node_repoints_terminal_node(tmp_path, monkeypatch):
    d = _folder(tmp_path, status="done")
    monkeypatch.setattr("fno.plan._migrate._node_status", lambda nid: "done")
    captured = {}
    monkeypatch.setattr(
        "fno.plan._migrate._set_node_plan_path",
        lambda nid, path: captured.update(nid=nid, path=path),
    )
    res = migrate_folder(d, update_node="ab-deadbeef")
    assert res.node_updated
    assert captured["nid"] == "ab-deadbeef"
    assert captured["path"] == str(res.new_doc_path)

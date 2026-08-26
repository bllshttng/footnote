"""x-9415: every graph mutation refreshes the configured public projections.

Positive-marker discipline throughout: each test asserts the artifact the
outcome produces (file content, mtime advance, stderr naming an offender),
never a bare absence. The one absence asserted (non-canonical graph skips
targets) is paired with the same mutation writing graph.json, proving the
mutator ran and chose to skip.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
from typing import Generator

import pytest

from fno.graph.store import locked_mutate_graph


def _write_graph(path: Path, entries: list[dict]) -> None:
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")


def _read_graph(path: Path) -> list[dict]:
    return json.loads(path.read_text(encoding="utf-8"))["entries"]


def _entry(eid: str, **kwargs) -> dict:
    base = {
        "id": eid,
        "title": eid,
        "type": "feature",
        "priority": "p2",
        "completed_at": None,
        "deferred_at": None,
        "session_id": None,
        "status": "ready",
        "blocked_by": [],
        "plan_path": None,
        "pr_url": None,
        "project": "fno",
        "created_at": "2026-01-01T00:00:00Z",
    }
    base.update(kwargs)
    return base


def _write_config(targets_toml: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Point FNO_GLOBAL_SETTINGS_PATH at a tmp global config.toml carrying the rows."""
    cfg = tmp_path / "config.toml"
    cfg.write_text(f"[backlog]\n{targets_toml}", encoding="utf-8")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(cfg))


@pytest.fixture(autouse=True)
def _isolate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[dict[str, Path], None, None]:
    """Pin HOME and the graph constants so a canonical-graph test never writes
    the operator's real ~/.fno/graph.{json,md,html} or vault targets."""
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path / "repo"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.graph._constants as gc
    import fno.paths as paths_mod

    for cache in ("_settings", "resolve_repo_root"):
        maybe = getattr(paths_mod, cache, None)
        if maybe is not None:
            try:
                maybe.cache_clear()  # type: ignore[attr-defined]
            except AttributeError:
                pass
    for attr in ("GRAPH_JSON", "GRAPH_MD", "GRAPH_HTML", "GRAPH_ARCHIVE_JSON"):
        try:
            delattr(gc, attr)
        except AttributeError:
            pass

    graph = tmp_path / "graph.json"
    paths = {
        "graph": graph,
        "md": tmp_path / "graph.md",
        "html": tmp_path / "graph.html",
        "target": tmp_path / "out" / "fno-backlog.html",
    }
    # Canonical by default: the mutator's GRAPH_JSON comparison resolves
    # against these pinned constants, so renders stay inside tmp_path.
    monkeypatch.setattr(gc, "GRAPH_JSON", graph)
    monkeypatch.setattr(gc, "GRAPH_MD", paths["md"])
    monkeypatch.setattr(gc, "GRAPH_HTML", paths["html"])
    yield paths
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def _mutate(graph: Path, entries: list[dict], new_title: str) -> None:
    _write_graph(graph, entries)
    locked_mutate_graph(
        graph,
        lambda nodes: nodes[0].__setitem__("title", new_title) or nodes,
    )


# ---------------------------------------------------------------------------
# Config model
# ---------------------------------------------------------------------------


def test_render_target_config_defaults_and_typo():
    from fno.config import RenderTargetConfig

    row = RenderTargetConfig.model_validate(
        {"path": "~/vault/fno-backlog.html", "project": "fno"}
    )
    assert row.projection == "backlog"
    with pytest.raises(Exception) as exc:
        RenderTargetConfig.model_validate(
            {"path": "~/v/x.html", "project": "fno", "projection": "loc"}
        )
    assert "roadmap" in str(exc.value)


def test_relative_target_path_rejected():
    from fno.config import RenderTargetConfig

    with pytest.raises(Exception) as exc:
        RenderTargetConfig.model_validate({"path": "fno-backlog.html", "project": "fno"})
    assert "absolute" in str(exc.value)


def test_misspelled_target_key_rejected():
    from fno.config import RenderTargetConfig

    with pytest.raises(Exception) as exc:
        RenderTargetConfig.model_validate(
            {"path": "~/v/x.html", "project": "fno", "projectio": "roadmap"}
        )
    assert "projectio" in str(exc.value)


def test_state_file_collision_rejected(_isolate):
    import fno.graph._constants as gc
    from fno.config import RenderTargetConfig

    with pytest.raises(Exception) as exc:
        RenderTargetConfig.model_validate({"path": str(gc.GRAPH_MD), "project": "fno"})
    assert "collides" in str(exc.value)
    for state_path in (
        gc.GRAPH_JSON,
        gc.GRAPH_ARCHIVE_JSON,
        gc.LEDGER_JSON,
        str(gc.GRAPH_JSON) + ".sha256",
    ):
        with pytest.raises(Exception):
            RenderTargetConfig.model_validate({"path": str(state_path), "project": "fno"})


def test_unreadable_sibling_no_false_alarm(_isolate, tmp_path, monkeypatch, capsys):
    """A corrupt legacy settings.yaml under a config.toml that fully defines
    the rows renders normally and does NOT warn may-be-disabled."""
    global_yaml = tmp_path / "settings.yaml"
    global_yaml.write_text("::not yaml at all::\n", encoding="utf-8")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(global_yaml))
    toml_sibling = tmp_path / "config.toml"
    toml_sibling.write_text(
        f'[backlog]\n[[backlog.render_targets]]\npath = "{_isolate["target"]}"\nproject = "fno"\n',
        encoding="utf-8",
    )
    _mutate(
        _isolate["graph"],
        [_entry("ab-sibling0", title="first title")],
        "second title",
    )
    assert "second title" in _isolate["target"].read_text(encoding="utf-8")
    assert "may be disabled" not in capsys.readouterr().err


def test_bad_row_skipped_good_row_renders(_isolate, tmp_path, monkeypatch, capsys):
    good = tmp_path / "out" / "good.html"
    _write_config(
        f'[[backlog.render_targets]]\npath = "relative.html"\nproject = "fno"\n'
        f'[[backlog.render_targets]]\npath = "{good}"\nproject = "fno"',
        tmp_path,
        monkeypatch,
    )
    _mutate(
        _isolate["graph"],
        [_entry("ab-goodrow0", title="first title")],
        "second title",
    )
    assert "second title" in good.read_text(encoding="utf-8")
    assert "skipping malformed backlog.render_targets row" in capsys.readouterr().err


def test_project_local_rows_warn_not_render(_isolate, tmp_path, monkeypatch, capsys):
    global_cfg = tmp_path / "config.toml"
    global_cfg.write_text("[backlog]\n", encoding="utf-8")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(global_cfg))
    local_cfg = tmp_path / "local-config.toml"
    local_cfg.write_text(
        f'[[backlog.render_targets]]\npath = "{_isolate["target"]}"\nproject = "fno"',
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(local_cfg))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    from fno.graph.roadmap_public import render_configured_targets

    render_configured_targets([])
    err = capsys.readouterr().err
    assert "project-local row(s) ignored" in err
    assert not _isolate["target"].exists()
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_render_targets_table_typo_degrades_to_empty(caplog):
    import logging

    from fno.config import BacklogBlock

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        block = BacklogBlock.model_validate({"render_targets": {"vault": {"path": "x"}}})
    assert block.render_targets == []
    assert "[[backlog.render_targets]]" in caplog.text


# ---------------------------------------------------------------------------
# Auto-render on mutation
# ---------------------------------------------------------------------------


def test_configured_target_written_on_mutation(_isolate, tmp_path, monkeypatch, capsys):
    _write_config(
        f'[[backlog.render_targets]]\npath = "{_isolate["target"]}"\nproject = "fno"',
        tmp_path,
        monkeypatch,
    )
    _mutate(
        _isolate["graph"],
        [_entry("ab-writeme0", title="first title", status="ready")],
        "second title",
    )
    text = _isolate["target"].read_text(encoding="utf-8")
    assert "second title" in text
    assert "public items" in text


def test_target_mtime_advances_within_mutation_call(_isolate, tmp_path, monkeypatch):
    _write_config(
        f'[[backlog.render_targets]]\npath = "{_isolate["target"]}"\nproject = "fno"',
        tmp_path,
        monkeypatch,
    )
    graph = _isolate["graph"]
    _write_graph(graph, [_entry("ab-mtime00", title="first title")])
    locked_mutate_graph(graph, lambda nodes: nodes)
    before = _isolate["target"].stat().st_mtime_ns
    locked_mutate_graph(graph, lambda nodes: nodes)
    after = _isolate["target"].stat().st_mtime_ns
    assert after > before


def test_leak_refusal_leaves_target_byte_identical(_isolate, tmp_path, monkeypatch, capsys):
    target = _isolate["target"]
    _write_config(
        f'[[backlog.render_targets]]\npath = "{target}"\nproject = "fno"',
        tmp_path,
        monkeypatch,
    )
    graph = _isolate["graph"]
    _write_graph(graph, [_entry("ab-leaky000", title="x-1234 leaks here")])
    locked_mutate_graph(graph, lambda nodes: nodes)
    assert not target.exists()
    # Seed the target with prior bytes, then mutate again: the refusal must
    # leave those bytes untouched.
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("PRIOR PUBLIC BYTES", encoding="utf-8")
    digest_before = hashlib.sha256(target.read_bytes()).hexdigest()

    def mutator(nodes):
        nodes[0]["priority"] = "p1"
        return nodes

    locked_mutate_graph(graph, mutator)

    assert hashlib.sha256(target.read_bytes()).hexdigest() == digest_before
    err = capsys.readouterr().err
    assert "leak gate refused" in err
    assert "x-1234" in err and "node-id" in err
    # The mutation itself must not be wedged by the public refusal.
    row = _read_graph(graph)[0]
    assert row["priority"] == "p1"


def test_empty_project_writes_valid_empty_projection(_isolate, tmp_path, monkeypatch, capsys):
    target = tmp_path / "out" / "empty.html"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("STALE BYTES FROM A DEAD PROJECT", encoding="utf-8")
    _write_config(
        f'[[backlog.render_targets]]\npath = "{target}"\nproject = "ghost"',
        tmp_path,
        monkeypatch,
    )
    _mutate(
        _isolate["graph"],
        [_entry("ab-elsewher", title="belongs elsewhere", project="fno")],
        "still elsewhere",
    )
    text = target.read_text(encoding="utf-8")
    assert "STALE BYTES" not in text
    assert "0 public items" in text
    # A project matching zero entries is also the typo'd-name signature, so
    # the empty write carries a loud warning rather than passing silently.
    assert "matches no graph entry" in capsys.readouterr().err


def test_unwritable_target_warns_and_completes(_isolate, tmp_path, monkeypatch, capsys):
    outdir = tmp_path / "locked-out"
    outdir.mkdir()
    target = outdir / "board.html"
    _write_config(
        f'[[backlog.render_targets]]\npath = "{target}"\nproject = "fno"',
        tmp_path,
        monkeypatch,
    )
    os.chmod(outdir, 0o500)
    try:
        _mutate(
            _isolate["graph"],
            [_entry("ab-lockme00", title="first title")],
            "written title",
        )
    finally:
        os.chmod(outdir, 0o700)
    err = capsys.readouterr().err
    assert "render target" in err and "PermissionError" in err
    row = _read_graph(_isolate["graph"])[0]
    assert row["title"] == "written title"


def test_non_canonical_graph_skips_targets(_isolate, tmp_path, monkeypatch):
    import fno.graph._constants as gc

    target = tmp_path / "out" / "board.html"
    _write_config(
        f'[[backlog.render_targets]]\npath = "{target}"\nproject = "fno"',
        tmp_path,
        monkeypatch,
    )
    # Point the canonical constant AWAY from the mutated graph: a tmp graph
    # must never write the operator's configured public targets.
    monkeypatch.setattr(gc, "GRAPH_JSON", tmp_path / "elsewhere" / "graph.json")
    _mutate(
        _isolate["graph"],
        [_entry("ab-tmpgraph", title="first title")],
        "mutated title",
    )
    row = _read_graph(_isolate["graph"])[0]
    assert row["title"] == "mutated title"
    assert not target.exists()


def test_roadmap_projection_target(_isolate, tmp_path, monkeypatch):
    target = tmp_path / "out" / "roadmap.html"
    _write_config(
        f'[[backlog.render_targets]]\npath = "{target}"\nproject = "fno"\n'
        'projection = "roadmap"',
        tmp_path,
        monkeypatch,
    )
    _mutate(
        _isolate["graph"],
        [_entry("ab-roadmap0", title="shipped work", completed_at="2026-01-02T00:00:00Z")],
        "shipped work",
    )
    text = target.read_text(encoding="utf-8")
    assert "fno roadmap" in text
    assert "shipped work" in text


def test_gate_is_scoped_to_the_targets_own_render_set(_isolate, tmp_path, monkeypatch, capsys):
    backlog_target = tmp_path / "out" / "backlog.html"
    roadmap_target = tmp_path / "out" / "roadmap.html"
    _write_config(
        f'[[backlog.render_targets]]\npath = "{backlog_target}"\nproject = "fno"\n'
        f'[[backlog.render_targets]]\npath = "{roadmap_target}"\nproject = "fno"\n'
        'projection = "roadmap"',
        tmp_path,
        monkeypatch,
    )
    # A Done title carrying a PR reference leaks in the roadmap projection
    # (Done column) but not the backlog projection (open statuses only).
    done = _entry(
        "ab-done0000",
        title="wrap up PR #12",
        completed_at="2026-01-02T00:00:00Z",
    )
    open_node = _entry("ab-open0000", title="clean open work")
    graph = _isolate["graph"]
    _write_graph(graph, [done, open_node])
    locked_mutate_graph(graph, lambda nodes: nodes)

    backlog_text = backlog_target.read_text(encoding="utf-8")
    assert "clean open work" in backlog_text
    assert "wrap up PR #12" not in backlog_text
    err = capsys.readouterr().err
    assert "leak gate refused" in err
    assert "ab-done0000" in err and "pr-reference" in err
    assert not roadmap_target.exists()

"""Tests for ``fno backlog capture tidy`` (Phase 2 / US2).

Covers the one-idempotent-pass contract:
  - AC3: eject filed ``ab-*`` lines whose node completed (read concrete fields,
    not ``status``); incomplete filed nodes stay.
  - AC5: ``tidy`` is idempotent (a second run is byte-identical to the first
    run's output) and the pinned digest lists every open ``#jc`` action exactly
    once, dated items ascending by date then undated in stable source order.
  - Separator migration (em-dash -> hyphen) is owned by ``tidy`` and only
    rewrites the item separator, never em-dashes inside prose.
  - Fail-safe when graph.json is unreadable (skip eject, still build digest).
  - Digest marker drift refuses rather than guessing.
  - Dedup is report-only (clusters reported, no line mutated by it).

Separators use the literal em-dash (matching the real file + existing
fixtures); the calendar emoji is written as a ``\\U`` escape so this source
stays ASCII there.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

runner = CliRunner()

CAL = "\U0001F4C5"  # 📅


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()


def _write_graph(path: Path, entries: list[dict]) -> None:
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")


def _header() -> str:
    return (
        "---\n"
        "title: Abilities backlog inbox\n"
        "---\n"
        "\n"
        "# Abilities backlog inbox\n"
        "\n"
    )


# ---------------------------------------------------------------------------
# AC3 - eject filed nodes whose graph node completed
# ---------------------------------------------------------------------------

def test_tidy_ejects_completed_filed_node(tmp_path: Path) -> None:
    """AC3: a production-shaped open ab- line whose node has completed_at moves
    to inbox-archive.md; a second ab- line whose node has neither completed_at
    nor superseded_by stays in place."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    archive = tmp_path / "inbox-archive.md"
    graph = tmp_path / "graph.json"

    inbox.write_text(
        _header()
        + "## 2026-06-03\n\n"
        + "- [ ] ab-12345678 — **wire the thing** (p2). shipped via PR#1 "
          "source: PR#1 filed: ab-12345678\n"
        + "- [ ] ab-87654321 — **still open work** (p2). source: PR#2 "
          "filed: ab-87654321\n",
        encoding="utf-8",
    )
    _write_graph(
        graph,
        [
            {"id": "ab-12345678", "completed_at": "2026-06-03T00:00:00+00:00",
             "superseded_by": None, "deferred_at": None},
            {"id": "ab-87654321", "completed_at": None,
             "superseded_by": None, "deferred_at": None},
        ],
    )

    result = tidy(inbox, archive_path=archive, graph_path=graph)

    assert result["ejected"] == 1
    body = inbox.read_text(encoding="utf-8")
    assert "ab-12345678" not in body          # completed -> ejected
    assert "ab-87654321" in body              # incomplete -> stays
    assert archive.exists()
    assert "ab-12345678" in archive.read_text(encoding="utf-8")


def test_tidy_ejects_on_superseded(tmp_path: Path) -> None:
    """A node with superseded_by set (but no completed_at) is also 'complete'."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    inbox.write_text(
        _header() + "## h\n\n- [ ] ab-aaaaaaaa — superseded (p2)\n",
        encoding="utf-8",
    )
    _write_graph(graph, [{"id": "ab-aaaaaaaa", "completed_at": None,
                          "superseded_by": "ab-bbbbbbbb"}])

    result = tidy(inbox, graph_path=graph)
    assert result["ejected"] == 1
    assert "ab-aaaaaaaa" not in inbox.read_text(encoding="utf-8")


def test_tidy_ejects_promoted_line_when_node_complete(tmp_path: Path) -> None:
    """A promoted ``[x] fu- ... -> ab-`` line whose target node completed ejects."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    inbox.write_text(
        _header()
        + "## h\n\n"
        + "- [x] fu-abc123 — did the thing (p2) -> ab-99887766\n"
        + "- [x] fu-def456 — other thing (p2) -> ab-11112222\n",
        encoding="utf-8",
    )
    _write_graph(
        graph,
        [
            {"id": "ab-99887766", "completed_at": "2026-06-03T00:00:00+00:00"},
            {"id": "ab-11112222", "completed_at": None, "superseded_by": None},
        ],
    )
    result = tidy(inbox, graph_path=graph)
    assert result["ejected"] == 1
    body = inbox.read_text(encoding="utf-8")
    assert "fu-abc123" not in body     # promoted+complete -> ejected
    assert "fu-def456" in body         # promoted but node open -> stays


# ---------------------------------------------------------------------------
# Fail-safe: graph unreadable / missing
# ---------------------------------------------------------------------------

def test_tidy_failsafe_when_graph_missing(tmp_path: Path) -> None:
    """No graph.json -> eject nothing (never archive a live item on a read miss),
    but still build the digest."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "absent-graph.json"  # does not exist
    inbox.write_text(
        _header() + "## h\n\n- [ ] ab-12345678 — would eject if graph read (p2)\n",
        encoding="utf-8",
    )
    result = tidy(inbox, graph_path=graph)
    assert result["ejected"] == 0
    assert "ab-12345678" in inbox.read_text(encoding="utf-8")
    # digest still rebuilt
    assert "inbox-digest:start" in inbox.read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# Separator normalization (em-dash -> hyphen), prose untouched
# ---------------------------------------------------------------------------

def test_tidy_normalizes_item_separator_only(tmp_path: Path) -> None:
    """The em-dash item separator becomes ' - '; an em-dash inside the title
    prose is preserved."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    inbox.write_text(
        _header()
        + "## h\n\n"
        + "- [ ] fu-abc123 — title with — an em dash inside (p1)\n",
        encoding="utf-8",
    )
    tidy(inbox, graph_path=graph)
    body = inbox.read_text(encoding="utf-8")
    # The separator after the token is now a hyphen ...
    assert "- [ ] fu-abc123 - title with" in body
    # ... but the em-dash inside the prose survives.
    assert "title with — an em dash inside" in body


# ---------------------------------------------------------------------------
# AC5 - idempotency + deterministic digest ordering
# ---------------------------------------------------------------------------

def test_tidy_is_idempotent(tmp_path: Path) -> None:
    """AC5: a second tidy with no edit between runs is byte-identical to the
    first run's OUTPUT (not the original input)."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [{"id": "ab-12345678",
                          "completed_at": "2026-06-03T00:00:00+00:00"}])
    inbox.write_text(
        _header()
        + "## 2026-06-03\n\n"
        + "Some narrative prose that mentions fu-bad999 in passing.\n\n"
        + "- [ ] fu-abc123 — open followup (p1)\n"
        + "  where: src/foo.py\n"
        + "  why: needs hardening\n"
        + "- [ ] cv-deadbeef — a carveout (p2)\n"
        + "- [ ] ab-12345678 — filed + done (p2)\n"
        + f"- [ ] act on the review #jc {CAL} 2026-06-10\n"
        + f"- [ ] earlier action #jc {CAL} 2026-06-05\n"
        + "- [ ] undated action #jc\n",
        encoding="utf-8",
    )

    tidy(inbox, graph_path=graph)
    first = inbox.read_text(encoding="utf-8")
    tidy(inbox, graph_path=graph)
    second = inbox.read_text(encoding="utf-8")
    assert first == second, "tidy must be idempotent on its own output"


def test_tidy_digest_jc_ordering_and_dedup(tmp_path: Path) -> None:
    """AC5: the #jc digest lists each open action once, dated ascending by date,
    then undated in stable source order."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    inbox.write_text(
        _header()
        + "## h\n\n"
        + f"- [ ] later thing #jc {CAL} 2026-06-20\n"
        + f"- [ ] earlier thing #jc {CAL} 2026-06-01\n"
        + "- [ ] zeta undated #jc\n"
        + "- [ ] alpha undated #jc\n"
        + f"- [ ] earlier thing #jc {CAL} 2026-06-01\n",  # duplicate of earlier
        encoding="utf-8",
    )
    tidy(inbox, graph_path=graph)
    body = inbox.read_text(encoding="utf-8")

    start = body.index("## Open #jc actions")
    end = body.index("## Open followups by priority")
    digest = body[start:end]

    # dated ascending: 06-01 before 06-20
    assert digest.index("earlier thing") < digest.index("later thing")
    # undated come after the dated block
    assert digest.index("later thing") < digest.index("zeta undated")
    # undated in stable SOURCE order (zeta appeared before alpha)
    assert digest.index("zeta undated") < digest.index("alpha undated")
    # deduped: 'earlier thing' appears exactly once in the digest
    assert digest.count("earlier thing") == 1


def test_tidy_followups_digest_lists_ids_by_priority(tmp_path: Path) -> None:
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    inbox.write_text(
        _header()
        + "## h\n\n"
        + "- [ ] fu-aaaaaa — one (p1)\n"
        + "- [ ] fu-bbbbbb — two (p1)\n"
        + "- [ ] fu-cccccc — three (p2)\n",
        encoding="utf-8",
    )
    tidy(inbox, graph_path=graph)
    body = inbox.read_text(encoding="utf-8")
    start = body.index("## Open followups by priority")
    digest = body[start:body.index("inbox-digest:end")]
    assert "fu-aaaaaa" in digest and "fu-bbbbbb" in digest
    assert "p1: 2" in digest
    assert "p2: 1" in digest


# ---------------------------------------------------------------------------
# Digest marker drift
# ---------------------------------------------------------------------------

def test_tidy_refuses_on_duplicate_digest_markers(tmp_path: Path) -> None:
    """Two digest blocks -> refuse rather than append a third or clobber prose."""
    from fno.backlog.capture import tidy, InboxValidationError

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    inbox.write_text(
        _header()
        + "<!-- inbox-digest:start -->\n## Open #jc actions\n<!-- inbox-digest:end -->\n\n"
        + "<!-- inbox-digest:start -->\n## Open #jc actions\n<!-- inbox-digest:end -->\n\n"
        + "## h\n\n- [ ] fu-abc123 — x (p1)\n",
        encoding="utf-8",
    )
    with pytest.raises(InboxValidationError):
        tidy(inbox, graph_path=graph)


# ---------------------------------------------------------------------------
# Dedup is report-only
# ---------------------------------------------------------------------------

def test_tidy_dedup_reports_clusters_without_mutating(tmp_path: Path) -> None:
    """Two open items with the same (where + title) are reported as a cluster;
    neither line is removed or struck (report-only in v1)."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    inbox.write_text(
        _header()
        + "## h\n\n"
        + "- [ ] fu-aaaaaa — harden the parser (p2)\n"
        + "  where: src/foo.py\n"
        + "- [ ] fu-bbbbbb — harden the parser (p2)\n"
        + "  where: src/foo.py\n",
        encoding="utf-8",
    )
    result = tidy(inbox, graph_path=graph)
    assert result["dedup_clusters"], "expected at least one dedup cluster"
    cluster_ids = {fid for cluster in result["dedup_clusters"] for fid in cluster}
    assert {"fu-aaaaaa", "fu-bbbbbb"} <= cluster_ids
    body = inbox.read_text(encoding="utf-8")
    assert "fu-aaaaaa" in body and "fu-bbbbbb" in body  # report-only: both remain


# ---------------------------------------------------------------------------
# Prose token never ejected / parsed (AC2 carry-over)
# ---------------------------------------------------------------------------

def test_tidy_leaves_prose_tokens_alone(tmp_path: Path) -> None:
    """An ab- token in a narrative paragraph (no checkbox) is never ejected even
    if that node is complete."""
    from fno.backlog.capture import tidy

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [{"id": "ab-12345678",
                          "completed_at": "2026-06-03T00:00:00+00:00"}])
    prose = "This paragraph mentions ab-12345678 in running text, not a checkbox.\n"
    inbox.write_text(_header() + "## h\n\n" + prose, encoding="utf-8")
    result = tidy(inbox, graph_path=graph)
    assert result["ejected"] == 0
    assert "ab-12345678" in inbox.read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# Durability: archive-before-truncate (review Finding 5)
# ---------------------------------------------------------------------------

def test_tidy_archive_failure_preserves_inbox_line(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The archive append must precede the inbox truncation, so a failing
    archive write leaves the ejected line in the inbox (recoverable on retry)
    rather than losing it from both files."""
    from fno.backlog import capture as inbox_mod

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [{"id": "ab-12345678",
                          "completed_at": "2026-06-03T00:00:00+00:00"}])
    inbox.write_text(
        _header() + "## h\n\n- [ ] ab-12345678 — done node (p2)\n",
        encoding="utf-8",
    )

    def boom(*_a: object, **_k: object) -> None:
        raise OSError("archive disk full")

    monkeypatch.setattr(inbox_mod, "_archive_append", boom)

    with pytest.raises(OSError):
        inbox_mod.tidy(inbox, graph_path=graph)

    # inbox NOT truncated: the ejected line survives for a later retry.
    assert "ab-12345678" in inbox.read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# Separator migration: legacy fu-only paths survive normalization (codex P1)
# ---------------------------------------------------------------------------

def test_tidy_normalized_fu_line_still_promotable(tmp_path: Path) -> None:
    """After tidy rewrites a legacy em-dash fu line to the hyphen separator, the
    fu-only paths (parse_items/list and promote_item, which key on _ITEM_RE)
    must still find it by id - else captured followups become unpromotable."""
    from fno.backlog.capture import tidy, promote_item, parse_items

    inbox = tmp_path / "inbox.md"
    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    inbox.write_text(
        _header() + "## h\n\n- [ ] fu-abc123 — promote me (p1)\n",
        encoding="utf-8",
    )

    tidy(inbox, graph_path=graph)
    body = inbox.read_text(encoding="utf-8")
    assert "- [ ] fu-abc123 - promote me" in body  # normalized to hyphen
    assert any(i["id"] == "fu-abc123" for i in parse_items(body))  # legacy parser sees it

    result = promote_item(inbox, "fu-abc123", graph_path=graph)  # legacy write path finds it
    assert result["status"] == "promoted"
    assert result["node_id"].startswith("ab-")


# ---------------------------------------------------------------------------
# CLI smoke
# ---------------------------------------------------------------------------

def test_cli_tidy_json_smoke(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """`inbox tidy --json` runs end-to-end and emits a summary object."""
    from fno.backlog import capture as inbox_mod

    graph = tmp_path / "graph.json"
    _write_graph(graph, [])
    monkeypatch.setattr(inbox_mod, "_graph_path_for_tidy", lambda: graph)

    path = inbox_mod._inbox_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        _header() + "## h\n\n- [ ] fu-abc123 — a followup (p1)\n",
        encoding="utf-8",
    )
    res = runner.invoke(inbox_mod.cli, ["tidy", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert "ejected" in payload and "dedup_clusters" in payload

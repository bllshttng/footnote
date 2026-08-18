"""Tests for `fno decide` - the durable decision record and its recovery query.

The record has three stores: the append-only ``operator_decision`` event in the
project journal (durability), the machine-wide ``decisions.jsonl`` index (the
reader's only source), and the projection onto the subject node's graph entry
(the node view). A record that is only greppable is not recoverable.

The defect these guard against is a write that succeeded and a read that could
not find it. So every recall assertion names a POSITIVE marker - the returned
``decision_id`` - never the absence of an error.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.decide.cli import decide_app

runner = CliRunner()


def _node(nid: str, **over) -> dict:
    base = {
        "id": nid,
        "title": f"node {nid}",
        "status": "ready",
        "type": "feature",
        "priority": "p2",
    }
    base.update(over)
    return base


@pytest.fixture
def index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """The machine-wide decision index, pinned into the sandbox.

    Every test takes this: without it a test writes to the developer's real
    ``~/.fno/decisions.jsonl``, and reads back whatever else is in there.
    """
    path = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr("fno.paths.decisions_jsonl", lambda: path)
    return path


@pytest.fixture
def tmp_graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text(
        json.dumps({"entries": [_node("x-7d94", slug="fold-the-inbox")]}, indent=2) + "\n"
    )
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # entries_with_archive resolves the archive through fno.paths, which is
    # graph_json().parent / "graph-archive.json"; pin both so the read-through
    # test stays hermetic.
    monkeypatch.setattr(
        "fno.paths.graph_archive_json", lambda: tmp_path / "graph-archive.json"
    )
    return g


@pytest.fixture
def root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A hermetic canonical root for the events journal (FNO_REPO_ROOT hook)."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    return tmp_path


def _events(root: Path) -> list[dict]:
    return [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def test_record_appends_the_event_and_projects_onto_the_node(root: Path, tmp_graph: Path, index: Path):
    """fno decide writes the event AND the graph projection."""
    res = runner.invoke(
        decide_app,
        [
            "--subject", "x-7d94",
            "--decision", "fold every project's inbox",
            "--rationale", "a fold is a read; you do not migrate before you can see",
            "--option", "fold first",
            "--option", "migrate first",
        ],
    )
    assert res.exit_code == 0, res.output
    did = res.stdout.strip().splitlines()[-1]
    assert did.startswith("d-"), "stdout carries the new decision id"

    events = [e for e in _events(root) if e["type"] == "operator_decision"]
    assert len(events) == 1
    data = events[0]["data"]
    assert data["decision_id"] == did
    assert data["subject"] == "x-7d94"
    assert data["decision"] == "fold every project's inbox"
    assert data["authority_source"]

    entry = json.loads(tmp_graph.read_text())["entries"][0]
    assert [d["decision_id"] for d in entry["decisions"]] == [did]
    assert entry["decisions"][0]["rationale"].startswith("a fold is a read")
    assert entry["decisions"][0]["options"] == ["fold first", "migrate first"]
    assert entry["decisions"][0]["superseded_by"] is None

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "options: fold first, migrate first" in listed.output


def test_list_returns_decisions_newest_first(root: Path, tmp_graph: Path, index: Path):
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "first"])
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "second"])

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert listed.output.index("second") < listed.output.index("first")

    as_json = runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"])
    payload = json.loads(as_json.stdout)
    assert [d["decision"] for d in payload["decisions"]] == ["second", "first"]


def test_supersession_marks_the_older_decision(root: Path, tmp_graph: Path, index: Path):
    """Two decisions on one subject order themselves; the older one
    is marked, not hidden."""
    first = runner.invoke(
        decide_app, ["--subject", "x-7d94", "--decision", "migrate now"]
    ).stdout.strip().splitlines()[-1]
    second = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "fold first", "--supersedes", first],
    )
    assert second.exit_code == 0, second.output

    entry = json.loads(tmp_graph.read_text())["entries"][0]
    by_id = {d["decision_id"]: d for d in entry["decisions"]}
    assert by_id[first]["superseded_by"] is not None
    assert by_id[first]["superseded_by"].startswith("d-")

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "superseded by" in listed.output, "the render marks the superseded row"


def test_list_survives_archiving_of_the_subject(root: Path, tmp_graph: Path, index: Path):
    """A decision recorded pre-archive is still listable post-archive
    through entries_with_archive."""
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold first"])
    entries = json.loads(tmp_graph.read_text())["entries"]
    archive = tmp_graph.parent / "graph-archive.json"
    archive.write_text(json.dumps({"entries": entries}) + "\n")
    tmp_graph.write_text(json.dumps({"entries": []}) + "\n")

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "fold first" in listed.output


def test_list_of_a_subject_with_nothing_on_record_is_a_successful_read(
    root: Path, tmp_graph: Path, index: Path
):
    """Exit 0, not 1. A read that answered "none" ran; only a read that could
    not run is a failure, and the two must not share an exit code."""
    listed = runner.invoke(decide_app, ["list", "--subject", "x-nope"])
    assert listed.exit_code == 0, listed.output
    assert "no decisions recorded for 'x-nope'" in listed.output


def test_record_without_a_resolvable_subject_still_writes_the_event(
    root: Path, tmp_graph: Path, index: Path
):
    """A subject that names a file or area, not a node, loses the projection
    but keeps the durable event; the verb says so on stderr."""
    res = runner.invoke(
        decide_app, ["--subject", "docs/architecture.md", "--decision", "keep it"]
    )
    assert res.exit_code == 0, res.output
    events = [e for e in _events(root) if e["type"] == "operator_decision"]
    assert len(events) == 1
    assert events[0]["data"]["subject"] == "docs/architecture.md"
    assert "no node" in res.output.lower() or "projection" in res.output.lower()


def test_decisions_default_applies_on_read_for_legacy_rows(tmp_path: Path):
    """The decisions default lives in _apply_graph_defaults, the one migration
    seam: a pre-decision graph row reads [] without a rewrite."""
    from fno.graph.store import _apply_graph_defaults

    entries = _apply_graph_defaults([{"id": "x-old", "title": "old", "status": "ready"}])
    assert entries[0]["decisions"] == []


# --- recall parity: the reader takes every subject the writer takes ---------


@pytest.mark.parametrize(
    "subject",
    ["x-7d94", "fold-the-inbox", "pr-923", "docs/foo.md", "the mail bus"],
    ids=["node-id", "slug", "pr", "path", "area"],
)
def test_recall_answers_every_subject_shape_the_help_promises(
    root: Path, tmp_graph: Path, index: Path, subject: str
):
    """The defect, named. `--help` says the subject may be a node id/slug, a
    file, or an area; the writer took all of them and the reader took one, so a
    ruling about `pr-923` was written, receipted, and lost.

    Asserts the returned decision_id comes back - a positive marker. Restore the
    graph-only reader and the pr, path and area cases fail.
    """
    written = runner.invoke(
        decide_app, ["--subject", subject, "--decision", f"ruling about {subject}"]
    )
    assert written.exit_code == 0, written.output
    did = written.stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", subject, "--json"]).stdout
    )
    assert did in [d["decision_id"] for d in payload["decisions"]]


@pytest.mark.parametrize(
    "recorded_as,queried_as",
    [
        ("Fold-The-Inbox", "x-7d94"),
        ("fold-the-inbox", "x-7d94"),
        ("x-7d94", "fold-the-inbox"),
    ],
    ids=["mixed-case-slug", "slug", "id-queried-by-slug"],
)
def test_two_spellings_of_one_node_answer_each_other(
    root: Path, tmp_graph: Path, index: Path, recorded_as: str, queried_as: str
):
    """BOTH sides expand, not just the query.

    The operator records under whatever spelling was in front of them, and the
    receipt then prints the canonical id as the way back. A reader that expands
    only the query sends them to a command that returns nothing, which is this
    PR's own defect wearing a different word.

    Both sides run through the SAME resolver, so whichever spellings it accepts,
    the writer and the reader accept the same set. The bare-hex tier is left out
    here on purpose: it depends on the configured node prefix, so it would test
    the resolver's config rather than this symmetry.
    """
    written = runner.invoke(
        decide_app, ["--subject", recorded_as, "--decision", "one ruling"]
    )
    did = written.stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", queried_as, "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == [did]


def test_recall_is_exact_never_a_prefix_match(root: Path, tmp_graph: Path, index: Path):
    """A decision about pr-92 must not answer a query for pr-921. Set
    membership on the recorded string, never a fuzzy match."""
    runner.invoke(decide_app, ["--subject", "pr-92", "--decision", "the short one"])
    on_921 = runner.invoke(decide_app, ["list", "--subject", "pr-921", "--json"])
    assert json.loads(on_921.stdout)["decisions"] == []

    on_92 = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-92", "--json"]).stdout
    )
    assert [d["decision"] for d in on_92["decisions"]] == ["the short one"]


def test_supersession_is_derived_from_index_rows_alone(
    root: Path, tmp_graph: Path, index: Path
):
    """The graph projection stamped superseded_by under the lock. For a subject
    that names no node there is no projection, so the reader must derive it."""
    first = runner.invoke(
        decide_app, ["--subject", "pr-922", "--decision", "merge it"]
    ).stdout.strip().splitlines()[-1]
    second = runner.invoke(
        decide_app,
        ["--subject", "pr-922", "--decision", "hold it", "--supersedes", first],
    )
    assert second.exit_code == 0, second.output
    newer = second.stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-922", "--json"]).stdout
    )
    by_id = {d["decision_id"]: d for d in payload["decisions"]}
    assert by_id[first]["superseded_by"] == newer
    assert by_id[newer]["superseded_by"] is None

    listed = runner.invoke(decide_app, ["list", "--subject", "pr-922"])
    assert f"[superseded by {newer}]" in listed.output


def test_a_subjectless_decision_is_reachable_only_without_a_subject(
    root: Path, tmp_graph: Path, index: Path
):
    """`fno outstanding clear --answer` on a question naming no node records a
    decision with subject=None. A subject-less list is the only way to it."""
    from fno.outstanding.cli import outstanding_app

    asked = runner.invoke(outstanding_app, ["ask", "which lane owns the retry?"])
    assert asked.exit_code == 0, asked.output
    qid = asked.stdout.strip().splitlines()[-1]

    cleared = runner.invoke(
        outstanding_app, ["clear", qid, "--answer", "the dispatcher owns it"]
    )
    assert cleared.exit_code == 0, cleared.output

    payload = json.loads(runner.invoke(decide_app, ["list", "--json"]).stdout)
    assert "the dispatcher owns it" in [d["decision"] for d in payload["decisions"]]
    assert payload["subject"] == "(all)"


def test_limit_caps_the_newest_and_zero_means_no_cap(
    root: Path, tmp_graph: Path, index: Path
):
    for n in range(4):
        runner.invoke(decide_app, ["--subject", "pr-900", "--decision", f"call {n}"])

    capped = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "pr-900", "--limit", "2", "--json"]
        ).stdout
    )
    assert [d["decision"] for d in capped["decisions"]] == ["call 3", "call 2"]

    uncapped = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "pr-900", "--limit", "0", "--json"]
        ).stdout
    )
    assert len(uncapped["decisions"]) == 4


# --- reindex: the records already on disk become readable -------------------


def test_reindex_recovers_journal_records_and_is_idempotent(
    root: Path, tmp_graph: Path, index: Path
):
    """The backfill is the whole point: without it the fix helps no record that
    already exists."""
    from fno.decide import reindex

    journal = root / ".fno" / "events.jsonl"
    for subject in ("pr-923", "pr-921", "x-6352-worktree"):
        runner.invoke(decide_app, ["--subject", subject, "--decision", f"on {subject}"])
    index.unlink()  # the state before the index existed: journal only

    counts = reindex(sources=[journal])
    assert counts["added"] == 3, counts
    for subject in ("pr-923", "pr-921", "x-6352-worktree"):
        payload = json.loads(
            runner.invoke(decide_app, ["list", "--subject", subject, "--json"]).stdout
        )
        assert [d["decision"] for d in payload["decisions"]] == [f"on {subject}"]

    again = reindex(sources=[journal])
    assert again["added"] == 0 and again["already"] == 3, again


def test_reindex_reads_one_journal_once_through_a_symlink(
    root: Path, tmp_graph: Path, index: Path, tmp_path: Path
):
    """A linked checkout points .fno/events.jsonl at the canonical file. The
    (st_dev, st_ino) dedupe is what keeps a 54 MB journal from being read once
    per name it is reachable under."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    index.unlink()

    journal = root / ".fno" / "events.jsonl"
    link = tmp_path / "linked-events.jsonl"
    link.symlink_to(journal)

    counts = reindex(sources=[journal, link])
    assert counts["added"] == 1, counts
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    assert len(payload["decisions"]) == 1


def test_reindex_recovers_a_projection_row_that_stored_no_subject(
    root: Path, tmp_graph: Path, index: Path
):
    """The oldest projection on this machine predates the subject field. The
    row lives ON the node, so the node is the subject; without that fallback
    the recovered decision answers no query at all."""
    from fno.decide import reindex

    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"] = [
        {
            "decision_id": "d-legacy1",
            "decision": "fold every project's inbox first",
            "decided_by": "operator",
            "ts": "2026-08-15T00:31:06.178560Z",
        }
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    assert reindex(sources=[])["added"] == 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == ["d-legacy1"]


def test_reindex_folds_every_project_root_the_graph_names(
    root: Path, tmp_graph: Path, index: Path, tmp_path: Path
):
    """A free-text decision recorded from another repo has no graph projection
    to recover it. A backfill that folds only the invoking repo leaves exactly
    the records this verb exists to find."""
    from fno.decide import _default_journals, reindex

    sibling = tmp_path / "other-repo"
    (sibling / ".fno").mkdir(parents=True)
    entries = json.loads(tmp_graph.read_text())["entries"]
    entries.append(_node("x-9999", cwd=str(sibling)))
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    from fno.decide import record_decision

    did = record_decision(
        decision="the sibling repo ruled this", subject="pr-777", events_root=sibling
    )["decision_id"]
    index.unlink()

    assert any(sibling in p.parents for p in _default_journals()), _default_journals()
    assert reindex()["added"] >= 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-777", "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == [did]


@pytest.mark.parametrize(
    "torn",
    ['{"type":"operator_decision","data":{"decision_id":"d-tru', '{"ts":"2026-'],
    ids=["tear-after-the-type", "tear-before-the-type"],
)
def test_a_damaged_index_row_is_skipped_but_never_skipped_silently(
    root: Path, tmp_graph: Path, index: Path, capsys, torn: str
):
    """A truncated append must not make an unreadable record and an empty one
    look the same. The good rows still come back.

    Both tear points, because a crash can end the line before the type string
    ever appears, and a substring prefilter would drop exactly that one without
    ever counting it.
    """
    from fno.decide import _read_index

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("a", encoding="utf-8") as fh:
        fh.write(torn + "\n")

    capsys.readouterr()
    rows = _read_index(index)
    err = capsys.readouterr().err
    assert [r["decision"] for r in rows] == ["merged"], "one bad row costs no others"
    assert "1 damaged row(s)" in err
    assert "fno decide reindex" in err


def test_reindex_drops_the_damaged_row_so_the_warning_can_clear(
    root: Path, tmp_graph: Path, index: Path, capsys
):
    """The index never rotates, so a torn line stays forever and reprints the
    same notice on every read. The recovery the notice names must succeed."""
    from fno.decide import _read_index, reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("a", encoding="utf-8") as fh:
        fh.write('{"type":"operator_decision","data":{"decision_id":"d-tru\n')

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert counts["repaired"] == 1, counts

    capsys.readouterr()
    rows = _read_index(index)
    assert [r["decision"] for r in rows] == ["merged"]
    assert "damaged row(s)" not in capsys.readouterr().err


def test_reindex_counts_a_journal_row_and_its_own_projection_once(
    root: Path, tmp_graph: Path, index: Path
):
    """A first backfill must not report rows as already indexed. The journal
    row and its projection are one decision seen twice in one run."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold first"])
    index.unlink()

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert (counts["added"], counts["already"]) == (1, 0), counts


def test_a_failed_index_write_names_reindex_and_never_a_retry(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """By the time the index write fails, the durable event has landed. Telling
    the operator to re-run would record one ruling twice."""
    import fno.events as events_mod

    real = events_mod.append_event

    def boom(event, events_path=None, **kw):
        if events_path is not None and Path(events_path) == index:
            raise OSError("read-only file system")
        return real(event, events_path=events_path, **kw)

    monkeypatch.setattr(events_mod, "append_event", boom)
    res = runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    assert res.exit_code == 1
    assert "fno decide reindex" in res.output
    assert "Do NOT re-run decide" in res.output
    assert "recorded d-" in res.output, "the id it already holds"


def test_a_legacy_projection_row_with_no_ts_sorts_oldest(
    root: Path, tmp_graph: Path, index: Path
):
    """The event builder stamps NOW, which would float a legacy ruling to the
    top of a list whose whole promise is newest-first."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "recent"])
    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"].append(
        {"decision_id": "d-nots1", "decision": "ancient", "decided_by": "operator"}
    )
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    assert reindex(sources=[])["added"] == 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision"] for d in payload["decisions"]] == ["recent", "ancient"]


def test_limit_says_so_when_it_truncates(root: Path, tmp_graph: Path, index: Path):
    """A silent cut on a recall verb is the same lie as a missing record."""
    for n in range(3):
        runner.invoke(decide_app, ["--subject", "pr-900", "--decision", f"call {n}"])

    payload = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "pr-900", "--limit", "2", "--json"]
        ).stdout
    )
    assert (payload["total"], payload["truncated"]) == (3, True)

    human = runner.invoke(decide_app, ["list", "--subject", "pr-900", "--limit", "2"])
    assert "showing 2 of 3" in human.output


def test_a_torn_multibyte_append_stays_readable_and_recoverable(
    root: Path, tmp_graph: Path, index: Path
):
    """A crash can split a multi-byte character mid-append. A strict read
    raises on the WHOLE file, taking every good row with it - and breaking the
    reindex the damaged-row warning names as the cure."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("ab") as fh:
        fh.write(b'{"type":"operator_decision","data":{"decision":"caf\xc3\n')

    listed = runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"])
    assert listed.exit_code == 0, listed.output
    assert [d["decision"] for d in json.loads(listed.stdout)["decisions"]] == ["merged"]

    assert reindex(sources=[root / ".fno" / "events.jsonl"])["repaired"] == 1
    assert index.with_suffix(".jsonl.corrupt").exists(), "the drop is reversible"


def test_one_unusable_projection_row_does_not_abort_the_backfill(
    root: Path, tmp_graph: Path, index: Path
):
    """The event builder slices strings and validates. An eager list build
    aborts on the first bad row and loses the journal half of the fold too."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "from the journal"])
    index.unlink()
    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"] = [
        {"decision_id": "d-bad001", "decision": "unusable", "rationale": 123},
        {"decision_id": "d-good01", "decision": "usable", "subject": "x-7d94"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert counts["added"] == 2, counts
    for subject, decision in (("pr-923", "from the journal"), ("x-7d94", "usable")):
        payload = json.loads(
            runner.invoke(decide_app, ["list", "--subject", subject, "--json"]).stdout
        )
        assert decision in [d["decision"] for d in payload["decisions"]]


def test_an_unreachable_index_is_a_failed_read_not_an_empty_one(
    root: Path, tmp_graph: Path, index: Path
):
    """Path.exists() answers False for a dangling symlink, which would turn an
    unreachable store into "no decisions recorded" on exit 0."""
    index.parent.mkdir(parents=True, exist_ok=True)
    index.symlink_to(index.parent / "gone.jsonl")

    listed = runner.invoke(decide_app, ["list", "--subject", "pr-923"])
    assert listed.exit_code == 1, listed.output
    assert "cannot read the decision index" in listed.output


def test_the_second_producer_also_refuses_to_ask_for_a_retry(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """`fno outstanding clear --answer` is the other operator_decision writer.
    A guard on one of two producer paths is decorative."""
    import fno.events as events_mod
    from fno.outstanding.cli import outstanding_app

    qid = runner.invoke(
        outstanding_app, ["ask", "which lane owns the retry?"]
    ).stdout.strip().splitlines()[-1]

    real = events_mod.append_event

    def boom(event, events_path=None, **kw):
        if events_path is not None and Path(events_path) == index:
            raise OSError("read-only file system")
        return real(event, events_path=events_path, **kw)

    monkeypatch.setattr(events_mod, "append_event", boom)
    res = runner.invoke(outstanding_app, ["clear", qid, "--answer", "the dispatcher"])
    assert res.exit_code == 1
    assert "fno decide reindex" in res.output
    assert "records the same ruling a second time" in res.output


def test_equal_timestamps_do_not_invert_newest_first(
    root: Path, tmp_graph: Path, index: Path
):
    """Every legacy projection row shares the same no-ts fallback, and a stable
    sort keeps file order for ties - silently reversing the stated contract."""
    from fno.decide import reindex

    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"] = [
        {"decision_id": "d-aaa001", "decision": "first", "subject": "x-7d94"},
        {"decision_id": "d-bbb002", "decision": "second", "subject": "x-7d94"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    assert reindex(sources=[])["added"] == 2

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == ["d-bbb002", "d-aaa001"]


def test_operator_decision_retention_is_durable_by_an_explicit_key():
    """It behaved this way only because it named no retention and the default
    is durable. The record the recall promise rests on is then one schema edit
    from being GC'd out of the project journal."""
    from fno.events import SCHEMA, retention_for

    assert retention_for("operator_decision") == "durable"
    entry = next(e for e in SCHEMA["event_types"] if e["name"] == "operator_decision")
    assert entry.get("retention") == "durable", "explicit, not inherited from the default"

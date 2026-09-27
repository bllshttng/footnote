"""Tests for the batch transcript-path answer the provider-cap actor reads.

The actor walks registry rows in Rust; this module is the one place that
answers "where is each row's transcript" through the same resolver
``fno agents peek`` reads transcripts with, so the two surfaces cannot
drift apart.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.agents.transcript_paths import resolve_paths

UUID = "d8996f9b-8854-4f22-8c28-c7819c6d0316"
SHORT = "d8996f9b"


def _transcript(projects: Path, uuid: str, body: str = "{}\n") -> Path:
    d = projects / "-repo"
    d.mkdir(parents=True, exist_ok=True)
    p = d / f"{uuid}.jsonl"
    p.write_text(body, encoding="utf-8")
    return p


def test_full_uuid_and_short_id_resolve_the_same_transcript(tmp_path: Path) -> None:
    projects = tmp_path / "projects"
    planted = _transcript(projects, UUID)

    resolved = resolve_paths([UUID, SHORT], projects_root=projects)

    assert resolved[UUID] == str(planted)
    # An 8-hex thread row carries only the short id; the resolver finds the
    # transcript by its prefix in the store, exactly as peek does, with no
    # sessions-dir hop in between.
    assert resolved[SHORT] == str(planted)


def test_miss_answers_none_and_never_raises(tmp_path: Path) -> None:
    resolved = resolve_paths(["ffffffff"], projects_root=tmp_path / "projects")
    assert resolved == {"ffffffff": None}


def test_dotted_sibling_artifact_is_never_the_answer(tmp_path: Path) -> None:
    projects = tmp_path / "projects"
    planted = _transcript(projects, UUID)
    (projects / "-repo" / f"{UUID}.orphaned-copy.jsonl").write_text("{}\n")

    resolved = resolve_paths([UUID], projects_root=projects)

    assert resolved[UUID] == str(planted)


def test_two_distinct_uuids_sharing_a_prefix_answer_sorted_first(tmp_path: Path) -> None:
    projects = tmp_path / "projects"
    first = _transcript(projects, f"{SHORT}-aaaa-4444-4444-444444444444")
    _transcript(projects, f"{SHORT}-bbbb-4444-4444-444444444444")

    resolved = resolve_paths([SHORT], projects_root=projects)

    # A genuinely ambiguous prefix answers the first-sorted match rather than
    # raising: this read is evidence for a cap tail, not an address to inject.
    assert resolved[SHORT] == str(first)


def test_cli_prints_one_json_map(tmp_path: Path) -> None:
    import typer
    from typer.testing import CliRunner

    from fno.agents.transcript_paths import cmd_transcript_paths

    projects = tmp_path / "projects"
    planted = _transcript(projects, UUID)

    app = typer.Typer()
    app.command()(cmd_transcript_paths)
    payload = json.dumps({"ids": [SHORT, "missing"], "projects_root": str(projects)})
    result = CliRunner().invoke(app, [], input=payload, catch_exceptions=False)

    assert result.exit_code == 0
    answered = json.loads(result.output)
    assert answered[SHORT] == str(planted)
    assert answered["missing"] is None

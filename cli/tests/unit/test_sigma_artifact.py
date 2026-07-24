from __future__ import annotations

from pathlib import Path

import yaml
from typer.testing import CliRunner

from fno.cli import app


def _report(path: Path, title: str) -> Path:
    path.write_text(f"# {title}\n\n- **P1** - bug\n", encoding="utf-8")
    return path


def _frontmatter(path: Path) -> dict[str, object]:
    text = path.read_text(encoding="utf-8")
    return yaml.safe_load(text.split("---", 2)[1])


def test_publish_writes_round_and_current_atomically(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact

    result = publish_sigma_artifact(
        _report(tmp_path / "report.md", "round one"),
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
    )

    assert result.published is True
    assert result.current_path == tmp_path / "reviews/fno/reviews/x-bfbb/sigma.md"
    assert result.round_path == tmp_path / "reviews/fno/reviews/x-bfbb/rounds/round-b.md"
    assert result.current_path.read_text() == result.round_path.read_text()
    assert _frontmatter(result.current_path)["head_sha"] == "head-b"


def test_stale_completion_retains_round_without_displacing_current(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact

    reviews_root = tmp_path / "reviews"
    current = publish_sigma_artifact(
        _report(tmp_path / "new.md", "new"),
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
    )
    stale = publish_sigma_artifact(
        _report(tmp_path / "old.md", "old"),
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-a",
        current_head="head-b",
        round_id="round-a",
    )

    assert stale.published is False
    assert stale.reason == "reviewed head is no longer current"
    assert stale.round_path.exists()
    assert current.current_path.read_text() == current.round_path.read_text()


def test_unknown_current_head_retains_round_without_publishing_alias(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact

    result = publish_sigma_artifact(
        _report(tmp_path / "report.md", "unknown"),
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-a",
        current_head=None,
        round_id="round-a",
    )

    assert result.published is False
    assert result.reason == "current head unavailable"
    assert result.round_path.exists()
    assert not result.current_path.exists()


def test_inspect_rejects_wrong_pr_or_head(tmp_path: Path) -> None:
    from fno.review.artifact import inspect_sigma_artifact, publish_sigma_artifact

    reviews_root = tmp_path / "reviews"
    publish_sigma_artifact(
        _report(tmp_path / "report.md", "current"),
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
    )

    assert (
        inspect_sigma_artifact(
            reviews_root=reviews_root,
            project="fno",
            node="x-bfbb",
            pr_number=99,
            head_sha="head-b",
        ).status
        == "rejected"
    )
    assert (
        inspect_sigma_artifact(
            reviews_root=reviews_root,
            project="fno",
            node="x-bfbb",
            pr_number=42,
            head_sha="head-c",
        ).status
        == "rejected"
    )


def test_inspect_accepts_current_artifact_with_zero_external_reviewers(tmp_path: Path) -> None:
    from fno.review.artifact import inspect_sigma_artifact, publish_sigma_artifact

    reviews_root = tmp_path / "reviews"
    publish_sigma_artifact(
        _report(tmp_path / "report.md", "current"),
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
    )

    inspected = inspect_sigma_artifact(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        head_sha="head-b",
    )
    assert inspected.status == "accepted"
    assert inspected.finding_count == 1
    assert inspected.round_id == "round-b"
    assert "**P1**" in inspected.body


def test_clean_report_headings_are_not_counted_as_findings(tmp_path: Path) -> None:
    from fno.review.artifact import inspect_sigma_artifact, publish_sigma_artifact

    report = tmp_path / "clean.md"
    report.write_text(
        "# Review\n\n### Critical Issues\n\nNone.\n\n### High Priority Issues\n\nNone.\n",
        encoding="utf-8",
    )
    reviews_root = tmp_path / "reviews"
    publish_sigma_artifact(
        report,
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
    )

    inspected = inspect_sigma_artifact(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        head_sha="head-b",
    )
    assert inspected.status == "accepted"
    assert inspected.finding_count == 0


def test_cli_publish_and_inspect_share_the_artifact_writer(tmp_path: Path) -> None:
    report = _report(tmp_path / "report.md", "current")
    reviews_root = tmp_path / "internal"
    published = CliRunner().invoke(
        app,
        [
            "review",
            "--publish-sigma",
            str(report),
            "--sigma-node",
            "x-bfbb",
            "--sigma-pr",
            "42",
            "--sigma-head",
            "head-b",
            "--sigma-current-head",
            "head-b",
            "--sigma-round",
            "round-b",
            "--sigma-project",
            "fno",
            "--sigma-reviews-root",
            str(reviews_root),
            "--json",
        ],
    )
    assert published.exit_code == 0, published.output
    assert '"published": true' in published.output.lower()

    inspected = CliRunner().invoke(
        app,
        [
            "review",
            "--inspect-sigma",
            "--sigma-node",
            "x-bfbb",
            "--sigma-pr",
            "42",
            "--sigma-head",
            "head-b",
            "--sigma-project",
            "fno",
            "--sigma-reviews-root",
            str(reviews_root),
            "--json",
        ],
    )
    assert inspected.exit_code == 0, inspected.output
    assert '"status": "accepted"' in inspected.output
    assert '"finding_count": 1' in inspected.output
    assert '"review_round": "round-b"' in inspected.output

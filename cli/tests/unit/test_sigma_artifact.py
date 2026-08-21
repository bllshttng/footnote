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
    assert result.finding_count == 1


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

    wrong_pr = inspect_sigma_artifact(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=99,
        head_sha="head-b",
    )
    wrong_head = inspect_sigma_artifact(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        head_sha="head-c",
    )
    assert wrong_pr.status == "rejected"
    assert wrong_pr.body == ""
    assert wrong_head.status == "rejected"
    assert wrong_head.body == ""


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


def test_inspect_rejects_missing_round_identity(tmp_path: Path) -> None:
    from fno.review.artifact import inspect_sigma_artifact

    path = tmp_path / "reviews/fno/reviews/x-bfbb/sigma.md"
    path.parent.mkdir(parents=True)
    path.write_text(
        "---\nschema: sigma-review/v1\nnode: x-bfbb\npr_number: 42\n"
        "head_sha: head-b\n---\n\n- **P1** - bug\n",
        encoding="utf-8",
    )
    result = inspect_sigma_artifact(
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
        head_sha="head-b",
    )
    assert result.status == "rejected"
    assert "review_round" in result.reason


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


def test_read_last_head_returns_stored_head(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact, read_sigma_last_head

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
        scope_base="head-a",
        scope_reason="incremental",
    )

    result = read_sigma_last_head(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
    )
    assert result.status == "found"
    assert result.head_sha == "head-b"


def test_read_last_head_rejects_reason_without_base(tmp_path: Path) -> None:
    from fno.review.artifact import read_sigma_last_head

    path = tmp_path / "reviews/fno/reviews/x-bfbb/sigma.md"
    path.parent.mkdir(parents=True)
    path.write_text(
        "---\nschema: sigma-review/v1\nnode: x-bfbb\npr_number: 42\n"
        "head_sha: head-b\nreview_round: round-b\n"
        "scope_reason: incremental\n---\n\n- **P1** - bug\n",
        encoding="utf-8",
    )
    result = read_sigma_last_head(
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
    )
    # A scope reason without its base proves nothing about cumulative
    # coverage; the pair is validated together or the head is not trusted.
    assert result.status == "rejected"
    assert result.head_sha is None
    assert "scope_base" in result.reason


def test_read_last_head_ignores_unscoped_artifacts(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact, read_sigma_last_head

    reviews_root = tmp_path / "reviews"
    publish_sigma_artifact(
        _report(tmp_path / "report.md", "single-commit panel"),
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
    )

    result = read_sigma_last_head(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=42,
    )
    # No scope fields means the writer did not resolve cumulative scope (the
    # internal single-commit panel publishes this artifact too); its head must
    # never narrow a later round, so it reads as no prior head.
    assert result.status == "unscoped"
    assert result.head_sha is None


def test_read_last_head_missing_artifact_is_not_found(tmp_path: Path) -> None:
    from fno.review.artifact import read_sigma_last_head

    result = read_sigma_last_head(
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
    )
    assert result.status == "missing"
    assert result.head_sha is None
    assert result.reason


def test_read_last_head_rejects_corrupt_frontmatter(tmp_path: Path) -> None:
    from fno.review.artifact import read_sigma_last_head

    path = tmp_path / "reviews/fno/reviews/x-bfbb/sigma.md"
    path.parent.mkdir(parents=True)
    path.write_text("no frontmatter at all\n", encoding="utf-8")
    result = read_sigma_last_head(
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
    )
    assert result.status == "rejected"
    assert result.head_sha is None
    assert result.reason


def test_read_last_head_rejects_node_or_pr_mismatch(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact, read_sigma_last_head

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

    wrong_node = read_sigma_last_head(
        reviews_root=reviews_root,
        project="fno",
        node="x-other",
        pr_number=42,
    )
    wrong_pr = read_sigma_last_head(
        reviews_root=reviews_root,
        project="fno",
        node="x-bfbb",
        pr_number=99,
    )
    # A wrong node resolves to a different artifact path, so it reads as
    # missing rather than mismatched; both are "no prior head" to a caller.
    assert wrong_node.status == "missing"
    assert wrong_node.head_sha is None
    assert wrong_pr.status == "rejected"
    assert wrong_pr.head_sha is None


def test_cli_sigma_last_head_prints_bare_sha(tmp_path: Path) -> None:
    report = _report(tmp_path / "report.md", "current")
    reviews_root = tmp_path / "internal"
    published = CliRunner().invoke(
        app,
        [
            "do", "review",
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
            "--sigma-scope-base",
            "head-a",
            "--sigma-scope-reason",
            "incremental",
            "--sigma-project",
            "fno",
            "--sigma-reviews-root",
            str(reviews_root),
        ],
    )
    assert published.exit_code == 0, published.output

    found = CliRunner().invoke(
        app,
        [
            "do", "review",
            "--sigma-last-head",
            "--sigma-node",
            "x-bfbb",
            "--sigma-pr",
            "42",
            "--sigma-project",
            "fno",
            "--sigma-reviews-root",
            str(reviews_root),
        ],
    )
    assert found.exit_code == 0, found.output
    assert found.stdout.strip() == "head-b"

    absent = CliRunner().invoke(
        app,
        [
            "do", "review",
            "--sigma-last-head",
            "--sigma-node",
            "x-nope",
            "--sigma-pr",
            "42",
            "--sigma-project",
            "fno",
            "--sigma-reviews-root",
            str(reviews_root),
        ],
    )
    assert absent.exit_code != 0
    assert absent.stdout.strip() == ""
    assert "sigma last head unavailable" in absent.stderr


def test_publish_carries_scope_into_frontmatter(tmp_path: Path) -> None:
    from fno.review.artifact import publish_sigma_artifact

    result = publish_sigma_artifact(
        _report(tmp_path / "report.md", "incremental"),
        reviews_root=tmp_path / "reviews",
        project="fno",
        node="x-bfbb",
        pr_number=42,
        reviewed_head="head-b",
        current_head="head-b",
        round_id="round-b",
        scope_base="head-a",
        scope_reason="incremental",
    )
    front = _frontmatter(result.current_path)
    assert front["scope_base"] == "head-a"
    assert front["scope_reason"] == "incremental"


def test_publish_rejects_half_and_unknown_scope(tmp_path: Path) -> None:
    import pytest

    from fno.review.artifact import publish_sigma_artifact

    with pytest.raises(ValueError, match="both base and reason"):
        publish_sigma_artifact(
            _report(tmp_path / "report.md", "half"),
            reviews_root=tmp_path / "reviews",
            project="fno",
            node="x-bfbb",
            pr_number=42,
            reviewed_head="head-b",
            current_head="head-b",
            round_id="round-b",
            scope_reason="incremental",
        )
    with pytest.raises(ValueError, match="invalid sigma artifact scope reason"):
        publish_sigma_artifact(
            _report(tmp_path / "report.md", "unknown reason"),
            reviews_root=tmp_path / "reviews",
            project="fno",
            node="x-bfbb",
            pr_number=42,
            reviewed_head="head-b",
            current_head="head-b",
            round_id="round-b",
            scope_base="head-a",
            scope_reason="totally-fine",
        )


def test_cli_publish_and_inspect_share_the_artifact_writer(tmp_path: Path) -> None:
    report = _report(tmp_path / "report.md", "current")
    reviews_root = tmp_path / "internal"
    published = CliRunner().invoke(
        app,
        [
            "do", "review",
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
    assert '"finding_count": 1' in published.output

    inspected = CliRunner().invoke(
        app,
        [
            "do", "review",
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

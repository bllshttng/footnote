"""Durable, head-pinned sigma review artifacts."""

from __future__ import annotations

import fcntl
import os
import re
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

import yaml

_SAFE_COMPONENT = re.compile(r"^[A-Za-z0-9._-]+$")

SCOPE_REASONS = (
    "first-round",
    "incremental",
    "rules-changed",
    "history-rewritten",
)


@dataclass(frozen=True)
class PublishResult:
    current_path: Path
    round_path: Path
    published: bool
    reason: str
    finding_count: int


@dataclass(frozen=True)
class InspectResult:
    path: Path
    status: str
    reason: str
    finding_count: int
    body: str
    round_id: str | None


@dataclass(frozen=True)
class LastHeadResult:
    status: str
    head_sha: str | None
    reason: str


def _component(value: str, label: str) -> str:
    if not value or not _SAFE_COMPONENT.fullmatch(value):
        raise ValueError(f"invalid sigma artifact {label}: {value!r}")
    return value


def _render(
    report: str,
    *,
    node: str,
    pr_number: int,
    reviewed_head: str,
    round_id: str,
    scope_base: str | None = None,
    scope_reason: str | None = None,
) -> str:
    metadata: dict[str, object] = {
        "schema": "sigma-review/v1",
        "node": node,
        "pr_number": pr_number,
        "head_sha": reviewed_head,
        "review_round": round_id,
        "generated_at": datetime.now(timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z"),
    }
    if scope_base is not None and scope_reason is not None:
        metadata["scope_base"] = scope_base
        metadata["scope_reason"] = scope_reason
    return (
        "---\n"
        + yaml.safe_dump(metadata, default_flow_style=False, sort_keys=False)
        + "---\n\n"
        + report.lstrip()
    )


def _atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, raw_tmp = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    tmp = Path(raw_tmp)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
    finally:
        tmp.unlink(missing_ok=True)


def publish_sigma_artifact(
    report_path: Path,
    *,
    reviews_root: Path,
    project: str,
    node: str,
    pr_number: int,
    reviewed_head: str,
    current_head: str | None,
    round_id: str,
    scope_base: str | None = None,
    scope_reason: str | None = None,
) -> PublishResult:
    """Retain a completed round and publish it only when its head is current."""
    project = _component(project, "project")
    node = _component(node, "node")
    round_id = _component(round_id, "round")
    reviewed_head = _component(reviewed_head, "head")
    if current_head is not None:
        current_head = _component(current_head, "current head")
    if (scope_base is None) != (scope_reason is None):
        raise ValueError("sigma artifact scope requires both base and reason")
    if scope_base is not None:
        scope_base = _component(scope_base, "scope base")
    if scope_reason is not None and scope_reason not in SCOPE_REASONS:
        raise ValueError(f"invalid sigma artifact scope reason: {scope_reason!r}")
    if pr_number < 1:
        raise ValueError("sigma artifact PR number must be positive")

    report = Path(report_path).read_text(encoding="utf-8")
    from fno.retro.harvest import extract_severity

    finding_count = sum(
        1 for line in report.splitlines() if extract_severity(line) is not None
    )
    rendered = _render(
        report,
        node=node,
        pr_number=pr_number,
        reviewed_head=reviewed_head,
        round_id=round_id,
        scope_base=scope_base,
        scope_reason=scope_reason,
    )
    node_dir = Path(reviews_root) / project / "reviews" / node
    round_path = node_dir / "rounds" / f"{round_id}.md"
    current_path = node_dir / "sigma.md"
    node_dir.mkdir(parents=True, exist_ok=True)

    lock_path = node_dir / ".publish.lock"
    with lock_path.open("a", encoding="utf-8") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        _atomic_write(round_path, rendered)
        if current_head is None:
            return PublishResult(
                current_path=current_path,
                round_path=round_path,
                published=False,
                reason="current head unavailable",
                finding_count=finding_count,
            )
        if reviewed_head != current_head:
            return PublishResult(
                current_path=current_path,
                round_path=round_path,
                published=False,
                reason="reviewed head is no longer current",
                finding_count=finding_count,
            )
        _atomic_write(current_path, rendered)

    return PublishResult(
        current_path=current_path,
        round_path=round_path,
        published=True,
        reason="published current reviewed head",
        finding_count=finding_count,
    )


def _split_artifact(text: str) -> tuple[dict[str, object], str]:
    if not text.startswith("---\n"):
        raise ValueError("missing sigma artifact frontmatter")
    pieces = text.split("---", 2)
    if len(pieces) != 3:
        raise ValueError("unterminated sigma artifact frontmatter")
    metadata = yaml.safe_load(pieces[1]) or {}
    if not isinstance(metadata, dict):
        raise ValueError("invalid sigma artifact frontmatter")
    return metadata, pieces[2].lstrip()


def read_sigma_last_head(
    *,
    reviews_root: Path,
    project: str,
    node: str,
    pr_number: int,
) -> LastHeadResult:
    """Read the stored head of the current artifact without expecting one.

    The inverse of inspect_sigma_artifact: that validates a caller-supplied
    head, while this answers "what head is stored?". Any non-found status is
    a normal input meaning "review everything", never an exception.
    """
    try:
        path = (
            Path(reviews_root)
            / _component(project, "project")
            / "reviews"
            / _component(node, "node")
            / "sigma.md"
        )
    except ValueError as exc:
        return LastHeadResult("rejected", None, str(exc))
    if not path.exists():
        return LastHeadResult("missing", None, "artifact does not exist")
    try:
        metadata, _body = _split_artifact(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, yaml.YAMLError) as exc:
        return LastHeadResult("rejected", None, str(exc))

    mismatches = [
        f"{key}={metadata.get(key)!r} (expected {value!r})"
        for key, value in (
            ("schema", "sigma-review/v1"),
            ("node", node),
            ("pr_number", pr_number),
        )
        if metadata.get(key) != value
    ]
    if mismatches:
        return LastHeadResult("rejected", None, "; ".join(mismatches))

    # Only a scope-aware round proves cumulative coverage up to its head: the
    # internal single-commit panel publishes this artifact too, and an artifact
    # without scope fields could narrow a later round past files never reviewed.
    reason = metadata.get("scope_reason")
    if reason not in SCOPE_REASONS:
        return LastHeadResult(
            "unscoped", None, f"artifact carries no valid scope reason: {reason!r}"
        )

    head = metadata.get("head_sha")
    # Same grammar the writer enforces: never surface a partial or placeholder
    # SHA, which a caller would narrow review scope against.
    if not isinstance(head, str) or not _SAFE_COMPONENT.fullmatch(head):
        return LastHeadResult("rejected", None, f"head_sha is missing or invalid: {head!r}")
    return LastHeadResult("found", head, "current artifact")


def inspect_sigma_artifact(
    *,
    reviews_root: Path,
    project: str,
    node: str,
    pr_number: int,
    head_sha: str,
) -> InspectResult:
    """Validate the deterministic artifact before the PR owner drains it."""
    path = (
        Path(reviews_root)
        / _component(project, "project")
        / "reviews"
        / _component(node, "node")
        / "sigma.md"
    )
    if not path.exists():
        return InspectResult(path, "missing", "artifact does not exist", 0, "", None)
    try:
        metadata, body = _split_artifact(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, yaml.YAMLError) as exc:
        return InspectResult(path, "rejected", str(exc), 0, "", None)

    expected = {
        "schema": "sigma-review/v1",
        "node": node,
        "pr_number": pr_number,
        "head_sha": head_sha,
    }
    mismatches = [
        f"{key}={metadata.get(key)!r} (expected {value!r})"
        for key, value in expected.items()
        if metadata.get(key) != value
    ]
    if not isinstance(metadata.get("review_round"), str) or not metadata.get(
        "review_round"
    ):
        mismatches.append("review_round is missing or invalid")
    if mismatches:
        return InspectResult(
            path,
            "rejected",
            "; ".join(mismatches),
            0,
            "",
            str(metadata.get("review_round")) if metadata.get("review_round") else None,
        )

    from fno.retro.harvest import extract_severity

    count = sum(1 for line in body.splitlines() if extract_severity(line) is not None)
    round_id = metadata.get("review_round")
    return InspectResult(
        path,
        "accepted",
        "current artifact",
        count,
        body,
        str(round_id) if round_id else None,
    )

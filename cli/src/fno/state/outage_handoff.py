"""Target-state ownership checks used by same-worktree handoffs."""
from __future__ import annotations

import hashlib
import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

from fno.schemas.target import TargetState
from fno.state.io import read_frontmatter


class ManifestAuthorityError(ValueError):
    """The live target manifest does not match the requested owner."""


class ManifestArchiveCollision(FileExistsError):
    """An attempt archive already exists with different bytes."""


@dataclass(frozen=True)
class ManifestAuthority:
    node: str
    claim_holder: str
    owner_cwd: str
    plan_path: str
    branch: str
    head: str
    worktree_id: str
    harness_session_id: str


@dataclass(frozen=True)
class ManifestArchiveReceipt:
    path: str
    content_hash: str


@dataclass(frozen=True)
class ManifestInspection:
    authority: ManifestAuthority
    content_hash: str


_SAFE_ATTEMPT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")


def _body_field(body: str, key: str) -> str:
    matches = []
    pattern = re.compile(rf"^[ \t]*{re.escape(key)}:[ \t]*(.*?)[ \t]*$", re.MULTILINE)
    for match in pattern.finditer(body):
        raw = match.group(1).strip()
        if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in "\"'":
            raw = raw[1:-1]
        matches.append(raw)
    if len(matches) != 1 or not matches[0]:
        raise ManifestAuthorityError(f"manifest body requires exactly one {key}")
    return matches[0]


def _git(cwd: Path, *args: str) -> str:
    try:
        return subprocess.check_output(
            ["git", *args], cwd=cwd, text=True, stderr=subprocess.DEVNULL
        ).strip()
    except (OSError, subprocess.CalledProcessError) as exc:
        raise ManifestAuthorityError(f"cannot resolve git {' '.join(args)}") from exc


def inspect_target_manifest(worktree: Path) -> ManifestInspection:
    """Read both manifest regions and bind them to current git identity."""
    worktree = worktree.resolve()
    state_path = worktree / ".fno" / "target-state.md"
    try:
        frontmatter, body = read_frontmatter(state_path)
        model = TargetState.model_validate(frontmatter)
        content = state_path.read_bytes()
    except (OSError, ValueError) as exc:
        raise ManifestAuthorityError(f"target manifest is unreadable or invalid: {exc}") from exc

    body_node = _body_field(body, "graph_node_id")
    body_key = _body_field(body, "target_claim_key")
    body_holder = _body_field(body, "target_claim_holder")
    if body_key != f"node:{body_node}":
        raise ManifestAuthorityError("manifest body claim key does not name its node")
    if not all((model.owner_cwd, model.plan_path, model.harness_session_id, body_holder)):
        raise ManifestAuthorityError("manifest is missing owner, plan, session, or claim authority")
    authority = ManifestAuthority(
        node=body_node,
        claim_holder=body_holder,
        owner_cwd=str(Path(model.owner_cwd).resolve()),
        plan_path=str(Path(model.plan_path).resolve()),
        branch=_git(worktree, "branch", "--show-current"),
        head=_git(worktree, "rev-parse", "HEAD"),
        worktree_id=str(Path(_git(worktree, "rev-parse", "--git-dir")).resolve()),
        harness_session_id=model.harness_session_id,
    )
    if Path(authority.owner_cwd) != worktree:
        raise ManifestAuthorityError("manifest owner_cwd is not this worktree")
    for key, expected in (
        ("graph_node_id", authority.node),
        ("target_claim_key", f"node:{authority.node}"),
        ("target_claim_holder", authority.claim_holder),
    ):
        if key in frontmatter and str(frontmatter[key]) != expected:
            raise ManifestAuthorityError(f"frontmatter {key} conflicts with body authority")
    return ManifestInspection(
        authority=authority,
        content_hash=hashlib.sha256(content).hexdigest(),
    )


def _validate(worktree: Path, authority: ManifestAuthority) -> tuple[Path, bytes]:
    worktree = worktree.resolve()
    state_path = worktree / ".fno" / "target-state.md"
    inspection = inspect_target_manifest(worktree)
    if inspection.authority != authority:
        raise ManifestAuthorityError("live manifest authority changed before archive")
    content = state_path.read_bytes()
    return state_path, content


def archive_target_manifest(
    worktree: Path,
    attempt: str,
    authority: ManifestAuthority,
) -> ManifestArchiveReceipt:
    """Validate and archive the immutable target manifest without overwrite."""
    if not _SAFE_ATTEMPT.fullmatch(attempt):
        raise ManifestAuthorityError("attempt id is not safe for an archive filename")
    state_path, content = _validate(Path(worktree), authority)
    digest = hashlib.sha256(content).hexdigest()
    archive_dir = Path(authority.plan_path + ".artifacts")
    archive_path = archive_dir / f"target-state-{attempt}.md"
    archive_dir.mkdir(parents=True, exist_ok=True)
    try:
        fd = os.open(archive_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        try:
            existing_hash = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        except OSError as exc:
            raise ManifestArchiveCollision(str(archive_path)) from exc
        if existing_hash != digest:
            raise ManifestArchiveCollision(str(archive_path))
    else:
        with os.fdopen(fd, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
    state_path.unlink()
    return ManifestArchiveReceipt(path=str(archive_path), content_hash=digest)

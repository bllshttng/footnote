"""Target-state ownership checks used by same-worktree handoffs."""
from __future__ import annotations

import hashlib
import argparse
import json
import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from fno.schemas.target import TargetState
from fno.state.io import read_frontmatter


class ManifestAuthorityError(ValueError):
    """The live target manifest does not match the requested owner."""


class ManifestArchiveCollision(FileExistsError):
    """An attempt archive already exists with different bytes."""


class ManifestPrepareError(RuntimeError):
    """Manifest custody was restored because exact claim release failed."""

    def __init__(
        self,
        reason: str,
        *,
        archive_path: str,
        restored: bool,
        live_holder: str | None = None,
    ) -> None:
        self.archive_path = archive_path
        self.restored = restored
        self.live_holder = live_holder
        super().__init__(reason)


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
class ManifestPrepareReceipt:
    path: str
    content_hash: str
    claim_released: bool = True


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
    owner_cwd, plan_path, harness_session_id = (
        model.owner_cwd,
        model.plan_path,
        model.harness_session_id,
    )
    if not (owner_cwd and plan_path and harness_session_id and body_holder):
        raise ManifestAuthorityError("manifest is missing owner, plan, session, or claim authority")
    authority = ManifestAuthority(
        node=body_node,
        claim_holder=body_holder,
        owner_cwd=str(Path(owner_cwd).resolve()),
        plan_path=str(Path(plan_path).resolve()),
        branch=_git(worktree, "branch", "--show-current"),
        head=_git(worktree, "rev-parse", "HEAD"),
        worktree_id=str(Path(_git(worktree, "rev-parse", "--git-dir")).resolve()),
        harness_session_id=harness_session_id,
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
    archive_dir = Path(authority.plan_path + ".artifacts")
    archive_path = archive_dir / f"target-state-{attempt}.md"
    digest = _archive_exact(state_path, archive_path, content)
    return ManifestArchiveReceipt(path=str(archive_path), content_hash=digest)


def _archive_exact(state_path: Path, archive_path: Path, content: bytes) -> str:
    """Persist exact bytes once, then remove the live manifest."""
    digest = hashlib.sha256(content).hexdigest()
    archive_path.parent.mkdir(parents=True, exist_ok=True)
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
    return digest


def _restore_target_manifest(
    worktree: Path, archive_path: Path, content_hash: str
) -> bool:
    """Restore identical bytes without replacing a concurrently-created file."""
    state_path = Path(worktree).resolve() / ".fno" / "target-state.md"
    try:
        content = archive_path.read_bytes()
    except OSError:
        return False
    if hashlib.sha256(content).hexdigest() != content_hash:
        return False
    try:
        fd = os.open(state_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        try:
            return hashlib.sha256(state_path.read_bytes()).hexdigest() == content_hash
        except OSError:
            return False
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
    except BaseException:
        try:
            state_path.unlink()
        except OSError:
            pass
        raise
    return True


def prepare_target_handoff(
    worktree: Path,
    attempt: str,
    authority: ManifestAuthority,
    *,
    release_exact: Callable[[str, str], object] | None = None,
    claims_root: Path | None = None,
) -> ManifestPrepareReceipt:
    """Archive the manifest and release only its exact claim, or restore custody."""
    if release_exact is None:
        from fno.claims.core import release_claim
        from fno.claims.io import claims_root_for

        def release_exact(key: str, holder: str) -> object:
            return release_claim(
                key,
                holder=holder,
                root=claims_root or claims_root_for(key),
                strict=True,
            )

    if not _SAFE_ATTEMPT.fullmatch(attempt):
        raise ManifestAuthorityError("attempt id is not safe for an archive filename")
    state_path, content = _validate(Path(worktree), authority)
    archive_path = Path(authority.plan_path + ".artifacts") / f"target-state-{attempt}.md"
    return prepare_manifest_and_release(
        state_path,
        archive_path,
        claim_key=f"node:{authority.node}",
        holder=authority.claim_holder,
        release_exact=release_exact,
    )


def prepare_manifest_and_release(
    state_path: Path,
    archive_path: Path,
    *,
    claim_key: str,
    holder: str,
    release_exact: Callable[[str, str], object],
) -> ManifestPrepareReceipt:
    """The sole archive plus exact-release operation for every handoff caller."""
    state_path = Path(state_path)
    archive_path = Path(archive_path)
    content = state_path.read_bytes()
    digest = _archive_exact(state_path, archive_path, content)
    try:
        released = release_exact(claim_key, holder)
        if released is False or released is None:
            raise RuntimeError("exact claim release was not positively confirmed")
    except Exception as exc:
        live_holder = getattr(exc, "actual", None)
        restored = _restore_target_manifest(
            state_path.parent.parent, archive_path, digest
        )
        raise ManifestPrepareError(
            f"exact claim release failed: {exc}",
            archive_path=str(archive_path),
            restored=restored,
            live_holder=str(live_holder) if live_holder else None,
        ) from exc
    return ManifestPrepareReceipt(str(archive_path), digest)


def _main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m fno.state.outage_handoff")
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare = subparsers.add_parser("prepare")
    prepare.add_argument("--state", type=Path, required=True)
    prepare.add_argument("--archive", type=Path, required=True)
    prepare.add_argument("--claim-key", required=True)
    prepare.add_argument("--holder", required=True)
    args = parser.parse_args(argv)

    def release_via_cli(key: str, holder: str) -> object:
        proc = subprocess.run(
            ["fno", "claim", "release", key, "--holder", holder],
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError((proc.stderr or proc.stdout or "claim release failed").strip())
        return True

    try:
        receipt = prepare_manifest_and_release(
            args.state,
            args.archive,
            claim_key=args.claim_key,
            holder=args.holder,
            release_exact=release_via_cli,
        )
    except ManifestPrepareError as exc:
        print(json.dumps({
            "ok": False,
            "reason": str(exc),
            "archive_path": exc.archive_path,
            "restored": exc.restored,
            "live_holder": exc.live_holder,
        }, sort_keys=True))
        return 10 if exc.restored else 12
    except Exception as exc:
        print(json.dumps({"ok": False, "reason": str(exc)}, sort_keys=True))
        return 10
    print(json.dumps({"ok": True, **receipt.__dict__}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())

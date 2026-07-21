"""Atomic file I/O for claim files.

Two operations matter:

    atomic_create_exclusive(path, content) - O_CREAT|O_EXCL write. If the
        path already exists, raise ClaimAlreadyHeld (low-level signal the
        caller's race-recovery path watches for).
    read_claim_file(path) - parse YAML into a Claim, raising ClaimGoneAway
        on missing-file and ClaimCorrupted on parse failure.

Key-to-path encoding uses urllib.parse.quote with safe='' so colon-separated
keys like "node:ab-1234" become "node%3Aab-1234.lock". This keeps filenames
portable across filesystems and lets ``ls .fno/claims/`` work without
shell escaping.

Claims are CROSS-WORKTREE coordination state. ``claims_dir()`` (with no
explicit root) resolves to the canonical repo root (main worktree) so that a
claim acquired from a conductor worktree and one acquired from the canonical
checkout land in the SAME directory, preserving the "at most one live
node:<id> holder at every instant" invariant across worktrees.
"""
from __future__ import annotations

import errno
import os
import tempfile
from pathlib import Path
from typing import Any
from urllib.parse import quote, unquote

import yaml

from .types import Claim


CLAIMS_DIRNAME = ".fno/claims"
EXPIRED_SUBDIR = ".expired"

# Env var naming the base dir under which GLOBAL (node-level) claims live.
# Node ids are global (like ~/.fno/graph.json), so a node:<id> lock must
# coordinate across worktrees rather than land in a worktree-local cwd. Shell
# call sites that manage node claims (init-target-state.sh; the deleted
# set-gate.sh / claim-release.sh) set this to "$HOME" so the claim lands at
# ~/.fno/claims. An older `fno` that predates this var silently ignores
# it (writes cwd-local, degraded but never erroring), which is why this is an
# env var and not a CLI flag. Per-root claims (the megawalk walker singleton)
# pass an explicit root and are unaffected.
CLAIMS_ROOT_ENV = "FNO_CLAIMS_ROOT"


class ClaimAlreadyHeld(Exception):
    """Raised by atomic_create_exclusive when the target path exists.

    Carries no contextual data; the caller is expected to re-read the
    existing file to decide between idempotent / stale-recovery / live-other.
    """


class ClaimCorrupted(Exception):
    """Raised when a claim file exists but cannot be parsed as YAML+schema."""


class ClaimGoneAway(Exception):
    """Raised when a claim file disappeared between read and use."""


def global_claims_root() -> Path:
    """Base dir for GLOBAL (node-level) claims.

    ``claims_dir(global_claims_root())`` is ``~/.fno/claims`` by default,
    a sibling of the global ``~/.fno/graph.json``. Honors
    ``$FNO_CLAIMS_ROOT`` so tests (and any path-config override) can
    redirect it; falls back to the user's home directory.
    """
    override = os.environ.get(CLAIMS_ROOT_ENV)
    return Path(override) if override else Path.home()


# Claim prefixes whose identifier is a globally-unique graph node id. Node ids
# are global (like ~/.fno/graph.json), so EVERY claim keyed on one must
# coordinate across worktrees/repos via the global root, never a cwd-local dir.
# - node:<id>      the canonical work-claim
# - dispatch:<id>  the boot-window bridge token (same id space as node:)
# - reconcile:<id> the merge-context sentinel (written in the blocker's repo,
#                  read in the dependent's repo)
# - session:<uuid> the single-writer guard for a claude session (G1 adopt, x-26df):
#                  a session is durable + cross-checkout, so two project checkouts
#                  must coordinate on the SAME lock or both could drive its
#                  transcript (codex P1).
# Keys whose identifier is a repo-local resource (walker:<repo_root>) embed
# their own scope and are NOT listed here; they keep the cwd/env default.
_GLOBAL_ID_PREFIXES = frozenset({"node", "dispatch", "reconcile", "session", "groom"})


def claims_root_for(key: str) -> Path | None:
    """Resolve the claims root for ``key`` by what its identifier refers to.

    A claim keyed on a globally-unique id (``node:``/``dispatch:``/
    ``reconcile:`` name the same global graph node; ``session:`` names a durable
    claude session; ``groom:<date>`` names a day of the global graph, so the
    daily pass dedups across repos) is rooted at the global
    ($HOME / ``$FNO_CLAIMS_ROOT``) root,
    so a writer and a reader in different repos/worktrees coordinate on the SAME
    lock. A claim keyed on a
    repo-local resource (``walker:<repo_root>``) or any unrecognized / colon-less
    key returns ``None`` -> the cwd/env default resolved by :func:`claims_dir`.

    This is the single source of truth the dispatch surfaces (``claims.cli``,
    ``backlog.advance``, ``backlog.reconcile_dispatch``, ``agents.cli``)
    delegate to, so their routing cannot drift.
    """
    # partition (not split) so a colon-less key equal to a prefix (e.g. the bare
    # token "node") does NOT match -- a global-id key is always "<prefix>:<id>".
    prefix, colon, _ = key.partition(":")
    return global_claims_root() if colon and prefix in _GLOBAL_ID_PREFIXES else None


def claims_dir(root: Path | None = None) -> Path:
    """Return the claims directory under the given repo root.

    Claims are cross-worktree coordination state. Resolution order for the
    base dir:

      1. explicit ``root`` argument (per-root claims, e.g. walker singleton),
      2. ``$FNO_CLAIMS_ROOT`` env var (global node claims; see
         ``CLAIMS_ROOT_ENV``),
      3. the canonical repo root (main worktree via
         :func:`fno.paths.resolve_canonical_repo_root`), so that claims
         from linked worktrees and the canonical checkout land in the same
         directory.
    """
    if root is not None:
        base: Path = root
    else:
        override = os.environ.get(CLAIMS_ROOT_ENV)
        if override:
            base = Path(override)
        else:
            from fno.paths import resolve_canonical_repo_root

            base = resolve_canonical_repo_root()
    return base / CLAIMS_DIRNAME


def encode_key(key: str) -> str:
    """URL-encode a key for use as a filename. Inverse of decode_key."""
    return quote(key, safe="")


def decode_key(filename: str) -> str:
    """Inverse of encode_key. Strips the .lock suffix if present."""
    if filename.endswith(".lock"):
        filename = filename[: -len(".lock")]
    return unquote(filename)


def claim_path(key: str, root: Path | None = None) -> Path:
    """Return the canonical file path for a claim key."""
    return claims_dir(root) / f"{encode_key(key)}.lock"


def expired_archive_path(key: str, ts_ms: int, root: Path | None = None) -> Path:
    """Return the archive path for a recovered stale claim."""
    return claims_dir(root) / EXPIRED_SUBDIR / f"{encode_key(key)}.{ts_ms}.lock"


def atomic_create_exclusive(path: Path, content: str) -> None:
    """Atomically create `path` with `content`, failing if it already exists.

    Writes the full content to a temp file in the SAME directory, then
    hardlinks it into place. `os.link` is atomic and raises FileExistsError
    when `path` already exists, so this keeps the exclusive-winner semantics
    of O_CREAT|O_EXCL while closing the TOCTOU window that a bare
    create-then-write leaves open: a concurrent reader (a losing acquirer
    inspecting the holder's file for liveness) now sees either no file at
    all or a fully-written one, never a created-but-empty file that would
    parse as ``ClaimCorrupted('root is not a dict')``.

    Raises ClaimAlreadyHeld if path already exists. Creates the parent
    directory on ENOENT and retries exactly once. Other OSErrors (ENOSPC,
    EACCES, ...) propagate; no partial file is left at `path`.
    """
    raw = content.encode("utf-8")

    def _attempt() -> None:
        # Temp file in the same directory keeps os.link on one filesystem
        # (cross-device hardlinks fail with EXDEV).
        fd, tmp_name = tempfile.mkstemp(prefix=".claim-tmp-", dir=str(path.parent))
        try:
            # os.write may short-write; loop until every byte lands. No fsync:
            # once the writes return, a concurrent reader on the same fs sees
            # the full content via the page cache, which is all the hardlink
            # publish needs (crash-durability isn't required for a lock file).
            view = memoryview(raw)
            while view:
                view = view[os.write(fd, view) :]
        finally:
            os.close(fd)
        try:
            os.link(tmp_name, str(path))  # atomic publish; EEXIST if held
        finally:
            try:
                os.unlink(tmp_name)
            except OSError:
                pass

    try:
        _attempt()
    except FileExistsError as exc:
        raise ClaimAlreadyHeld(str(path)) from exc
    except FileNotFoundError:
        # Parent directory missing. Create it once and retry.
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            _attempt()
        except FileExistsError as exc:
            raise ClaimAlreadyHeld(str(path)) from exc
        # ENOSPC etc. propagate to the caller.
    except OSError as exc:
        if exc.errno == errno.EEXIST:
            raise ClaimAlreadyHeld(str(path)) from exc
        raise


def serialize_claim(claim: Claim) -> str:
    """Convert a Claim to its YAML string for on-disk storage."""
    data = claim.to_yaml_dict()
    return yaml.safe_dump(data, sort_keys=False)


def parse_claim_dict(raw: dict[str, Any]) -> Claim:
    """Build a Claim from a parsed dict; raises ClaimCorrupted on validation error."""
    try:
        return Claim.model_validate(raw)
    except Exception as exc:
        raise ClaimCorrupted(f"claim schema validation failed: {exc}") from exc


def read_claim_file(path: Path) -> Claim:
    """Read and parse a claim file.

    Raises:
        ClaimGoneAway: the file disappeared between caller-decision and read.
        ClaimCorrupted: file is unparseable YAML or fails schema validation.
    """
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise ClaimGoneAway(str(path)) from exc

    try:
        data = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise ClaimCorrupted(f"YAML parse failed for {path}: {exc}") from exc

    if not isinstance(data, dict):
        raise ClaimCorrupted(f"claim file root is not a dict: {path}")

    return parse_claim_dict(data)


def archive_claim(path: Path, ts_ms: int) -> Path:
    """Move a (stale or force-released) claim file into the .expired/ archive.

    Returns the archive path. Idempotent: a missing source file is treated
    as already-archived. The archive name is suffixed with ts_ms so multiple
    archives of the same key do not collide.
    """
    if not path.exists():
        return path
    key = decode_key(path.name)
    archive = expired_archive_path(key, ts_ms, root=path.parent.parent.parent)
    archive.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.rename(str(path), str(archive))
    except FileNotFoundError:
        # Another process archived first; harmless.
        pass
    return archive

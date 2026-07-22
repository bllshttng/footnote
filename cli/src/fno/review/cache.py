"""git_sha-keyed result cache for idempotent review re-runs.

Cache results keyed by ``session_id + git_sha(HEAD) + prompt_hash``.
A second ``fno review`` on an unchanged tree returns the cached artifact
instead of re-spawning 6 workers.

Cache layout::

    <artifacts_dir>/review-cache/<key>.md

File format::

    ---
    phase: review
    cache_key: <key>
    session_id: <sid>
    git_sha: <sha>
    findings_count: N
    ---
    # Findings (JSON)
    <JSON-serialized list[Finding dict]>
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Iterable

if TYPE_CHECKING:
    from fno.review.orchestrator import OrchestratorResult


# ---------------------------------------------------------------------------
# Key derivation
# ---------------------------------------------------------------------------

def cache_key(
    session_id: str,
    git_sha: str,
    prompt_hash_value: str,
    provider_set: Iterable[str] | None = None,
) -> str:
    """Stable SHA-256 of the inputs joined with ASCII record-separators.

    The record-separator character (\\x1e) is used between fields so that
    adjacent field values cannot collide (e.g., ``ab`` + ``c`` != ``a`` + ``bc``).

    ``provider_set`` is the cross-model dimension (ab-6c8f4c61): the sorted set
    of per-agent resolved provider kinds for this run. A cross-model run would
    otherwise collide with an all-claude entry for the same SHA. A falsy
    ``provider_set`` (None or empty) appends NOTHING, reproducing today's exact
    key so existing all-claude cache entries still hit (back-compat).

    Returns:
        64-character lowercase hex string.
    """
    raw = f"{session_id}\x1e{git_sha}\x1e{prompt_hash_value}"
    if provider_set:
        raw += "\x1e" + ",".join(sorted(set(provider_set)))
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


def prompt_hash(prompts: dict[str, str]) -> str:
    """Stable SHA-256 hash of the agent->prompt mapping.

    Sorted by agent name so insertion order does not affect the key.
    JSON-serialized with ``sort_keys=True, ensure_ascii=False`` for stability.

    Returns:
        64-character lowercase hex string.
    """
    items = sorted(prompts.items())
    canonical = json.dumps(items, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


# ---------------------------------------------------------------------------
# git_sha helper
# ---------------------------------------------------------------------------

def git_sha(repo_path: Path | None = None) -> str:
    """Return the current HEAD commit SHA.

    Args:
        repo_path: Directory to use as the git working tree. Defaults to
            the current working directory (appropriate for production; tests
            pass ``tmp_path`` to isolate).

    Returns:
        40-character hex SHA string on success.
        ``"empty-tree"`` when inside a git repo but there are no commits yet.
        ``"unknown"`` when not inside a git repo at all.
    """
    cwd = str(repo_path) if repo_path is not None else None
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            cwd=cwd,
        )
    except FileNotFoundError:
        # git not available on this machine.
        return "unknown"

    if result.returncode == 0:
        return result.stdout.strip()

    stderr = result.stderr.lower()
    if "unknown revision" in stderr or "ambiguous argument" in stderr:
        # Inside a git repo but no commits yet.
        return "empty-tree"

    # Not a git repo (fatal: not a git repository) or other error.
    return "unknown"


# ---------------------------------------------------------------------------
# Path helpers
# ---------------------------------------------------------------------------

def cache_path(key: str, *, artifacts_dir: Path) -> Path:
    """Return the cache file path for the given key.

    Does NOT create the parent directory; that is done by ``write_cache``.
    """
    return artifacts_dir / "review-cache" / f"{key}.md"


# ---------------------------------------------------------------------------
# Read / write
# ---------------------------------------------------------------------------

def read_cache(key: str, *, artifacts_dir: Path) -> str | None:
    """Return the cached body text, or None on miss or error.

    All I/O errors are swallowed and logged to stderr so a corrupt cache
    never crashes the orchestrator - it just falls through to a fresh run.
    """
    path = cache_path(key, artifacts_dir=artifacts_dir)
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None  # expected state for a cache miss; no log
    except Exception as exc:  # noqa: BLE001 - intentional broad catch
        print(f"[cache] read error ({key[:8]}...): {exc}", file=sys.stderr)
        return None


def write_cache(key: str, body: str, *, artifacts_dir: Path) -> None:
    """Write body to the cache file atomically.

    Uses ``fno.state.io.atomic_write`` if available; otherwise falls
    back to a manual tmpfile + os.replace approach.

    The cache directory is created on first write.
    """
    path = cache_path(key, artifacts_dir=artifacts_dir)
    path.parent.mkdir(parents=True, exist_ok=True)

    try:
        from fno.state.io import atomic_write as _atomic_write
        _atomic_write(path, body)
        return
    except ImportError:
        pass

    # Fallback: manual atomic write.
    tmp = path.with_suffix(f".tmp.{os.getpid()}")
    try:
        tmp.write_text(body, encoding="utf-8")
        os.replace(str(tmp), str(path))
    except Exception:
        # Clean up the tmp file if something went wrong.
        try:
            tmp.unlink(missing_ok=True)
        except Exception:
            pass
        raise


# ---------------------------------------------------------------------------
# Serialization / deserialization
# ---------------------------------------------------------------------------

def build_cache_body(
    key: str,
    session_id: str,
    git_sha_value: str,
    result: "OrchestratorResult",
    provider_set: Iterable[str] | None = None,
) -> str:
    """Serialize an OrchestratorResult to the cache body format.

    Format::

        ---
        phase: review
        cache_key: <key>
        session_id: <sid>
        git_sha: <sha>
        findings_count: N
        workers_completed: N
        workers_failed: N
        provider_set: claude,codex   # only when cross-model engaged
        ---
        # Findings (JSON)
        <JSON-serialized list[Finding dict]>

    A falsy ``provider_set`` (None/empty) omits the ``provider_set:`` line so a
    legacy all-claude body is byte-for-byte identical to pre-cross-model output
    (back-compat with cache entries written before ab-6c8f4c61).
    """
    findings_list = [dataclasses.asdict(f) for f in result.findings]
    findings_json = json.dumps(findings_list, ensure_ascii=False, sort_keys=True)

    frontmatter_lines = [
        "---",
        "phase: review",
        f"cache_key: {key}",
        f"session_id: {session_id}",
        f"git_sha: {git_sha_value}",
        f"findings_count: {len(result.findings)}",
        f"workers_completed: {result.workers_completed}",
        f"workers_failed: {result.workers_failed}",
    ]
    if provider_set:
        frontmatter_lines.append(
            f"provider_set: {','.join(sorted(set(provider_set)))}"
        )
    frontmatter_lines.append("---")
    frontmatter = "\n".join(frontmatter_lines)
    return f"{frontmatter}\n# Findings (JSON)\n{findings_json}\n"


def reconstruct_result(body: str) -> "OrchestratorResult":
    """Deserialize a cache body into an OrchestratorResult.

    Raises:
        ValueError: if the body cannot be parsed (caller should fall through
            to a fresh run).
    """
    from fno.review.orchestrator import Finding, OrchestratorResult

    # Split on the closing --- of the frontmatter.
    parts = body.split("---\n", 2)
    if len(parts) < 3:
        raise ValueError("cache body missing YAML frontmatter delimiters")

    frontmatter_text = parts[1]
    rest = parts[2]

    # Parse simple key: value frontmatter (no PyYAML dependency).
    meta: dict[str, str] = {}
    for line in frontmatter_text.strip().splitlines():
        if ":" in line:
            k, _, v = line.partition(":")
            meta[k.strip()] = v.strip()

    workers_completed = int(meta.get("workers_completed", "0"))
    workers_failed = int(meta.get("workers_failed", "0"))

    # Extract JSON body (after the "# Findings (JSON)" header line).
    json_start = rest.find("[")
    if json_start == -1:
        findings_raw: list[dict] = []
    else:
        findings_raw = json.loads(rest[json_start:])

    # Filter to only the fields the Finding dataclass accepts.
    valid_fields = {f.name for f in dataclasses.fields(Finding)}
    findings = [
        Finding(**{k: v for k, v in d.items() if k in valid_fields})
        for d in findings_raw
    ]

    # Reconstruct suspicious flag: all workers succeeded but no findings.
    suspicious = workers_completed > 0 and not findings

    return OrchestratorResult(
        findings=findings,
        workers_completed=workers_completed,
        workers_failed=workers_failed,
        suspicious=suspicious,
        duration_seconds=0.0,  # not meaningful for a cache hit
    )

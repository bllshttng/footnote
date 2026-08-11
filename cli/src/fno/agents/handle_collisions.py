"""Report generated-handle collisions in local Codex and OpenCode corpora."""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
import time
from collections import Counter
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from fno.agents.discover import (
    _codex_session_meta,
    default_codex_sessions_dir,
    default_opencode_db_path,
    default_opencode_storage_dir,
    opencode_connect,
)
from fno.agents.fs_scan import path_exists_strict, scan_files
from fno.harness_identity import canonical_handle, legacy_suffix_handle


class CorpusUnavailable(RuntimeError):
    """A requested corpus cannot support a complete measurement."""


@dataclass(frozen=True)
class CollisionMetrics:
    colliding_handle_count: int
    affected_session_count: int
    worst_cluster: int


@dataclass(frozen=True)
class ScopeMetrics:
    session_count: int
    legacy: CollisionMetrics
    canonical: CollisionMetrics


@dataclass(frozen=True)
class HarnessReport:
    full: ScopeMetrics
    window: ScopeMetrics


def _unavailable(harness: str, detail: str) -> CorpusUnavailable:
    return CorpusUnavailable(f"{harness} corpus unavailable: {detail}")


def _remember(records: dict[str, float], session_id: str, created_at: float) -> None:
    records[session_id] = min(created_at, records.get(session_id, created_at))


def _iso_epoch(raw: object) -> float:
    if not isinstance(raw, str) or not raw:
        raise ValueError
    value = datetime.fromisoformat(raw)
    return value.replace(tzinfo=value.tzinfo or timezone.utc).timestamp()


def _millis_epoch(raw: object) -> float:
    if isinstance(raw, bool) or not isinstance(raw, (int, float)):
        raise ValueError
    return float(raw) / 1000


def _codex_records(root: Path) -> dict[str, float]:
    if not root.is_dir():
        raise _unavailable("codex", f"directory not found: {root}")
    try:
        paths = sorted(
            scan_files(
                root,
                include=lambda name: name.startswith("rollout-")
                and name.endswith(".jsonl"),
            )
        )
    except OSError as exc:
        raise _unavailable("codex", str(exc)) from exc
    if not paths:
        raise _unavailable("codex", f"no rollout files in {root}")

    records: dict[str, float] = {}
    for path in paths:
        payload = _codex_session_meta(path)
        if payload is None:
            raise _unavailable("codex", f"unreadable or malformed record: {path}")
        session_id = payload.get("id")
        if not isinstance(session_id, str) or not session_id:
            raise _unavailable("codex", f"unreadable or malformed record: {path}")
        try:
            _remember(records, session_id, _iso_epoch(payload.get("timestamp")))
        except (TypeError, ValueError, OverflowError) as exc:
            raise _unavailable("codex", f"invalid timestamp: {path}") from exc
    return records


def _opencode_db_records(path: Path) -> dict[str, float]:
    con = opencode_connect(path)
    if con is None:
        raise _unavailable("opencode", f"cannot read {path}")
    try:
        rows = list(con.execute("SELECT id, time_created FROM session ORDER BY id"))
    except sqlite3.Error as exc:
        raise _unavailable("opencode", f"cannot query {path}: {exc}") from exc
    finally:
        con.close()
    if not rows:
        raise _unavailable("opencode", f"no sessions in {path}")

    records: dict[str, float] = {}
    for session_id, created in rows:
        if not isinstance(session_id, str) or not session_id:
            raise _unavailable("opencode", f"invalid session id in {path}")
        try:
            _remember(records, session_id, _millis_epoch(created))
        except ValueError as exc:
            raise _unavailable("opencode", f"invalid time_created for {session_id}") from exc
    return records


def _opencode_legacy_records(root: Path) -> dict[str, float]:
    session_root = root / "session"
    if not session_root.is_dir():
        raise _unavailable("opencode", f"database and legacy store not found: {root}")
    try:
        paths = sorted(
            scan_files(
                session_root,
                max_depth=1,
                include=lambda name: name.endswith(".json"),
            )
        )
    except OSError as exc:
        raise _unavailable("opencode", str(exc)) from exc
    if not paths:
        raise _unavailable("opencode", f"no legacy sessions in {root}")

    records: dict[str, float] = {}
    for path in paths:
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
            session_id = payload["id"]
            created = payload["time"]["created"]
            if not isinstance(session_id, str) or not session_id:
                raise ValueError
            _remember(records, session_id, _millis_epoch(created))
        except (KeyError, OSError, TypeError, ValueError, UnicodeDecodeError) as exc:
            raise _unavailable("opencode", f"unreadable or malformed record: {path}") from exc
    return records


def _collision_metrics(
    session_ids: Sequence[str], handle_fn: Callable[[str], str]
) -> CollisionMetrics:
    sizes = list(Counter(handle_fn(session_id) for session_id in session_ids).values())
    collisions = [size for size in sizes if size > 1]
    return CollisionMetrics(len(collisions), sum(collisions), max(sizes, default=0))


def _scope(session_ids: Sequence[str], current: Callable[[str], str]) -> ScopeMetrics:
    return ScopeMetrics(
        len(session_ids),
        _collision_metrics(session_ids, legacy_suffix_handle),
        _collision_metrics(session_ids, current),
    )


def _report(
    records: dict[str, float], window_days: int, now: float, current: Callable[[str], str]
) -> HarnessReport:
    full = sorted(records)
    cutoff = now - window_days * 86_400
    window = [session_id for session_id in full if records[session_id] >= cutoff]
    return HarnessReport(_scope(full, current), _scope(window, current))


def run_census(
    *,
    codex_sessions_dir: Path,
    opencode_db_path: Path,
    opencode_storage_dir: Path,
    window_days: int,
    now: float,
    canonical_fn: Callable[[str], str] = canonical_handle,
) -> dict[str, HarnessReport]:
    """Read both corpora once and measure full and discovery-window scopes."""
    codex = _codex_records(codex_sessions_dir)
    try:
        has_opencode_db = path_exists_strict(opencode_db_path)
    except OSError as exc:
        raise _unavailable("opencode", str(exc)) from exc
    opencode = (
        _opencode_db_records(opencode_db_path)
        if has_opencode_db
        else _opencode_legacy_records(opencode_storage_dir)
    )
    return {
        "codex": _report(codex, window_days, now, canonical_fn),
        "opencode": _report(opencode, window_days, now, canonical_fn),
    }


def _line(harness: str, scope: str, metrics: ScopeMetrics) -> str:
    return (
        f"{harness} {scope} sessions={metrics.session_count} "
        f"legacy_colliding_handles={metrics.legacy.colliding_handle_count} "
        f"legacy_affected_sessions={metrics.legacy.affected_session_count} "
        f"legacy_worst_cluster={metrics.legacy.worst_cluster} "
        f"canonical_colliding_handles={metrics.canonical.colliding_handle_count} "
        f"canonical_affected_sessions={metrics.canonical.affected_session_count} "
        f"canonical_worst_cluster={metrics.canonical.worst_cluster}"
    )


def _positive_days(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("window days must be an integer") from exc
    if value <= 0:
        raise argparse.ArgumentTypeError("window days must be positive")
    return value


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--window-days", type=_positive_days, default=7)
    parser.add_argument("--codex-sessions-dir", type=Path, default=default_codex_sessions_dir())
    parser.add_argument("--opencode-db", type=Path, default=default_opencode_db_path())
    parser.add_argument(
        "--opencode-storage-dir", type=Path, default=default_opencode_storage_dir()
    )
    return parser


def main(
    argv: Sequence[str] | None = None,
    *,
    now: float | None = None,
    canonical_fn: Callable[[str], str] | None = None,
) -> int:
    args = _parser().parse_args(argv)
    try:
        reports = run_census(
            codex_sessions_dir=args.codex_sessions_dir,
            opencode_db_path=args.opencode_db,
            opencode_storage_dir=args.opencode_storage_dir,
            window_days=args.window_days,
            now=time.time() if now is None else now,
            canonical_fn=canonical_fn or canonical_handle,
        )
    except CorpusUnavailable as exc:
        print(f"collision census unavailable: {exc}", file=sys.stderr)
        return 2
    for harness, report in reports.items():
        print(_line(harness, "full", report.full))
        print(_line(harness, f"window-{args.window_days}d", report.window))
    return int(any(report.window.canonical.colliding_handle_count for report in reports.values()))


if __name__ == "__main__":
    raise SystemExit(main())

"""State-change dedupe with a rate floor for operator notifications (x-5f06).

The token IS the state; the tick is only a sample. Collapsing on the token is
what keeps main flipping red then green inside two hours from sending one push
per rerun attempt: the same token answers ``deduped`` and sends nothing, and a
changed token inside ``min_interval_s`` is held (not dropped - the token stays
unwritten, so the next pass reconsiders the same change).
"""
from __future__ import annotations

import json
import logging
import os
from datetime import datetime, timezone

_log = logging.getLogger(__name__)


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _load(path) -> dict:
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _write(path, state: dict) -> None:
    tmp = f"{path}.{os.getpid()}.tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(state, fh)
    os.replace(tmp, path)


def _min_interval_s() -> int:
    try:
        from fno.config import load_settings

        return int(load_settings().notify.min_interval_s)
    except Exception:  # noqa: BLE001 - config trouble degrades to the default floor
        return 300


def _age_seconds(ts: str) -> float:
    try:
        then = datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except (TypeError, ValueError):
        return float("inf")  # unreadable ts: treat as old, never hold forever
    return (datetime.now(timezone.utc) - then).total_seconds()


def notify_signal(
    key: str, state: str, title: str, body: str, pointer: str
) -> tuple[int, str]:
    """Send one notification collapsed on ``state``, or explain why not.

    Returns ``(code, verdict)``: ``(0, "deduped")`` when the stored token
    already equals ``state``, ``(0, "rate-held")`` when it changed inside the
    floor (the change retries next pass), otherwise the
    ``send_notification`` result for the send that went out.
    """
    from fno.notify._impl import send_notification
    from fno.paths import notify_signals_json

    path = notify_signals_json()
    store = _load(path)
    entry = store.get(key)
    if isinstance(entry, dict) and entry.get("token") == state:
        return 0, "deduped"
    if (
        isinstance(entry, dict)
        and _age_seconds(str(entry.get("ts", ""))) < _min_interval_s()
    ):
        return 0, "rate-held"

    # Send first, commit only on an accepted send: a notice that never left
    # the machine must not leave state saying it did - the next pass retries.
    code, err = send_notification(title, body, pointer)
    if code != 0:
        return code, err or "send_failed"

    store[key] = {"token": state, "ts": _now_iso()}
    try:
        _write(path, store)
    except OSError as exc:
        _log.warning("notify: signal state write failed: %s", exc)
    return 0, "sent"


def forget(key: str) -> None:
    """Drop one signal's stored state, so the next change sends again.

    The empty side of a signal (queue drained, runs gone) calls this instead of
    notifying: silence about nothing is the designed quiet.
    """
    from fno.paths import notify_signals_json

    path = notify_signals_json()
    store = _load(path)
    if key in store:
        del store[key]
        try:
            _write(path, store)
        except OSError as exc:
            _log.warning("notify: signal state write failed: %s", exc)

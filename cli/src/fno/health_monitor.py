"""Backlog health monitoring: thresholds, notifications, and history.

The triage health verb (``cli/src/fno/graph/triage.py::cmd_health``)
emits a deterministic report of backlog state (idea pile depth, stale ready
nodes, failure-prone nodes, collisions). This module turns that report into
a pass/fail signal: it evaluates the report against configurable thresholds,
dispatches breach notifications to configured surfaces, and appends a JSONL
history entry for trend analysis.

Design principle: deterministic until breach. The check verb runs no LLM
calls, costs zero, and exits 0 silently when everything is within thresholds.
Only when a threshold is crossed does it emit output. This is what makes the
check loop-safe: you can run it every hour without spam.
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass, asdict
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Any, Iterable, Literal, Optional

from fno import paths as _paths


# Severity levels.
Severity = Literal["info", "warn", "alert"]


# Notification surfaces.
Surface = Literal["terminal", "discord", "webhook", "log_only"]


# Metric keys recognized by evaluate_thresholds. Used to type-constrain
# Breach.key so a typo at a _make_breach call site fails type-check.
MetricKey = Literal[
    "idea_pile_depth",
    "stale_ready_nodes",
    "failure_prone_nodes",
    "collisions",
    "project_cwd_mismatch",
]


# Breach kind: count-based threshold vs presence-based filter.
BreachKind = Literal["count", "presence"]


# Default config block applied when settings.yaml omits keys. Derived from the
# Pydantic HealthMonitorBlock so the model is the single source of truth: this
# is the model's defaults, NOT an independent definition.
def _default_config() -> dict[str, Any]:
    from fno.config import HealthMonitorBlock

    return HealthMonitorBlock().model_dump()


DEFAULT_CONFIG: dict[str, Any] = _default_config()


@dataclass
class Breach:
    """A single threshold breach found in a health report.

    ``actual`` and ``threshold`` are stored as floats for consistent
    serialization. ``severity`` is derived from the overshoot ratio
    (count-based) or absolute count (presence-based). ``key`` is
    constrained to ``MetricKey`` so a typo at a ``_make_breach`` call
    site fails type-check rather than silently creating a new throttle
    slot.
    """

    key: MetricKey
    actual: float
    threshold: float
    severity: Severity
    message: str

    def __post_init__(self) -> None:
        # Defense-in-depth: callers always go through _make_breach which
        # validates upstream, but a direct Breach(...) construction (e.g.
        # in a test) shouldn't be able to ship nonsense severity or
        # negative counts that downstream throttle / display code assumes
        # away.
        if self.severity not in ("info", "warn", "alert"):
            raise ValueError(f"Invalid severity {self.severity!r}")
        if self.actual < 0:
            raise ValueError(f"actual must be non-negative, got {self.actual}")

    def to_jsonable(self) -> dict[str, Any]:
        return asdict(self)


# ---------------------------------------------------------------------------
# Config loading
# ---------------------------------------------------------------------------


def load_config(
    project_settings: Optional[Path] = None,
    user_settings: Optional[Path] = None,
) -> dict[str, Any]:
    """Load merged ``config.health_monitor`` config as a dict.

    Single source of truth is the Pydantic ``HealthMonitorBlock``: this reads
    ``load_settings().config.health_monitor`` (the standard cached,
    layered loader) and returns its ``model_dump()`` so every key the old
    DEFAULT_CONFIG carried is present and the dict shape is unchanged for
    downstream consumers (``evaluate_thresholds`` etc.).

    ``project_settings`` / ``user_settings`` are honored for callers (tests)
    that point at explicit temp files: they are merged through the SAME model
    via ``config.settings_from_files`` (project beats user), so there is no
    private merge or default set here anymore.
    """
    from fno.config import load_settings, settings_from_files

    try:
        if project_settings is None and user_settings is None:
            return load_settings().config.health_monitor.model_dump()
        explicit = [p for p in (project_settings, user_settings) if p is not None]
        return settings_from_files(explicit).config.health_monitor.model_dump()
    except Exception as exc:
        # Fail-open: a malformed UNRELATED setting (e.g. a bad state_dir glob)
        # makes full-model validation raise; health checks must still run with
        # defaults rather than abort. The old dedicated parser only read
        # config.health_monitor and ignored everything else.
        print(
            f"health_monitor: settings validation failed ({exc}); "
            "using default health config",
            file=sys.stderr,
        )
        return _default_config()


# ---------------------------------------------------------------------------
# Threshold evaluation
# ---------------------------------------------------------------------------


def _classify_severity(
    *, actual: float, threshold: float, kind: BreachKind
) -> Severity:
    """Derive severity from overshoot magnitude.

    ``kind="count"``: ratio = actual / max(threshold, 1). When threshold
    is exactly 0, treat any breach as alert-magnitude (the user's "zero
    is the target" signal would otherwise downgrade to info).

    ``kind="presence"``: ratio = actual (absolute count - no meaningful
    threshold to scale against; threshold value here is the upstream
    filter, not a breach cutoff).

    Brackets: ratio < 2.0 -> info; 2.0 <= ratio < 5.0 -> warn;
    ratio >= 5.0 -> alert.
    """
    if kind == "count":
        if threshold <= 0:
            ratio = 5.0  # any breach of a zero threshold is full severity
        else:
            ratio = actual / threshold
    elif kind == "presence":
        ratio = float(actual)
    else:
        # Unreachable under MetricKey/BreachKind type-check; raise so a
        # silent fallback doesn't mask a future call-site typo.
        raise ValueError(f"Unknown BreachKind: {kind!r}")
    if ratio >= 5.0:
        return "alert"
    if ratio >= 2.0:
        return "warn"
    return "info"


def _make_breach(
    key: MetricKey,
    *,
    actual: float,
    threshold: float,
    kind: BreachKind,
    hint: str = "",
) -> Breach:
    severity = _classify_severity(actual=actual, threshold=threshold, kind=kind)
    msg = (
        f"{key}: actual={actual}, threshold={threshold}"
        + (f" ({hint})" if hint else "")
    )
    return Breach(
        key=key,
        actual=float(actual),
        threshold=float(threshold),
        severity=severity,
        message=msg,
    )


def evaluate_thresholds(
    report: dict[str, Any],
    config: Optional[dict[str, Any]] = None,
) -> list[Breach]:
    """Compare a triage health report to configured thresholds.

    Returns the list of breach records. An empty list means "all green."
    When ``config`` is omitted, ``load_config()`` is called (project +
    user settings).

    Threshold semantics per key:
      - ``idea_pile_depth``: count threshold. Breach if
        ``report["idea_pile_depth"] > threshold``.
      - ``stale_ready_nodes`` / ``failure_prone_nodes``: presence-based.
        Breach if the list is non-empty. Threshold value in config is the
        filter applied at report-build time (passed via ``--stale-days``
        for the stale filter; failure_prone is currently hardcoded in
        cmd_health to ``len(sessions) > 1``).
      - ``collisions``: count threshold. Breach if
        ``len(report["collisions"]) > threshold``.
      - ``project_cwd_mismatch``: count threshold (default 0). Breach if
        ``report["project_cwd_mismatch"] > threshold``. With filing fixed,
        any mismatch on pending nodes is a producer regression. Any breach
        of the zero threshold is classified as alert-magnitude.
    """
    if config is None:
        config = load_config()
    if not config.get("enabled", True):
        return []
    thresh = config.get("thresholds", {})
    breaches: list[Breach] = []

    # 1. idea pile depth (count > N)
    idea_actual = int(report.get("idea_pile_depth", 0))
    idea_thresh = thresh.get("idea_pile_depth", DEFAULT_CONFIG["thresholds"]["idea_pile_depth"])
    if idea_actual > idea_thresh:
        breaches.append(_make_breach(
            "idea_pile_depth",
            actual=idea_actual,
            threshold=idea_thresh,
            kind="count",
            hint="idea-status nodes accumulating",
        ))

    # 2. stale ready (any presence)
    stale_count = len(report.get("stale_ready_nodes", []))
    if stale_count > 0:
        # threshold here is the *days* filter that produced the list, kept
        # for transparency in the breach record. Severity is based on count.
        stale_days_filter = thresh.get(
            "stale_ready_days", DEFAULT_CONFIG["thresholds"]["stale_ready_days"]
        )
        breaches.append(_make_breach(
            "stale_ready_nodes",
            actual=stale_count,
            threshold=stale_days_filter,
            kind="presence",
            hint=f"ready nodes older than {stale_days_filter}d",
        ))

    # 3. failure-prone (any presence)
    fp_count = len(report.get("failure_prone_nodes", []))
    if fp_count > 0:
        fp_attempts = thresh.get(
            "failure_prone_attempts",
            DEFAULT_CONFIG["thresholds"]["failure_prone_attempts"],
        )
        breaches.append(_make_breach(
            "failure_prone_nodes",
            actual=fp_count,
            threshold=fp_attempts,
            kind="presence",
            hint=f">={fp_attempts} attempts, no PR",
        ))

    # 4. collisions (count > N)
    coll_count = len(report.get("collisions", []))
    coll_thresh = thresh.get(
        "collision_count", DEFAULT_CONFIG["thresholds"]["collision_count"]
    )
    if coll_count > coll_thresh:
        breaches.append(_make_breach(
            "collisions",
            actual=coll_count,
            threshold=coll_thresh,
            kind="count",
            hint="medium+ collisions",
        ))

    # 5. project<->cwd mismatch (count > N; default threshold 0)
    mismatch_count = int(report.get("project_cwd_mismatch", 0))
    mismatch_thresh = thresh.get(
        "project_cwd_mismatch", DEFAULT_CONFIG["thresholds"]["project_cwd_mismatch"]
    )
    if mismatch_count > mismatch_thresh:
        breaches.append(_make_breach(
            "project_cwd_mismatch",
            actual=mismatch_count,
            threshold=mismatch_thresh,
            kind="count",
            hint="project/cwd disagree on pending nodes; producer regression?",
        ))

    return breaches


# ---------------------------------------------------------------------------
# Notification dispatch
# ---------------------------------------------------------------------------


def _default_throttle_path() -> Path:
    return _paths.state_dir() / "health-throttle.json"


def _default_alert_log_path() -> Path:
    return _paths.state_dir() / "health-alerts.log"


def _read_throttle_state(path: Path) -> dict[str, str]:
    """Read the throttle state JSON.

    Distinguishes "missing" (legitimate empty: no prior breaches) from
    "unreadable/corrupt" (operator-visible warning + treat as empty so
    the dispatch keeps working). A non-dict JSON payload (e.g. a list
    from manual editing) is also treated as corrupt rather than
    crashing the caller with AttributeError on .get().
    """
    if not path.exists():
        return {}
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as exc:
        print(
            f"health_monitor: throttle state at {path} unreadable ({exc}); "
            f"treating as empty (notifications may re-fire once)",
            file=sys.stderr,
        )
        return {}
    try:
        data = json.loads(raw)
    except ValueError as exc:
        print(
            f"health_monitor: throttle state at {path} is not valid JSON ({exc}); "
            f"treating as empty",
            file=sys.stderr,
        )
        return {}
    if not isinstance(data, dict):
        print(
            f"health_monitor: throttle state at {path} is not a dict "
            f"({type(data).__name__}); treating as empty",
            file=sys.stderr,
        )
        return {}
    return data


def _write_throttle_state(path: Path, state: dict[str, str]) -> None:
    """Atomic-write the throttle state.

    Uses a pid-disambiguated tmp path so concurrent writers do not
    overwrite each other's tmp file mid-rename.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(state, indent=2), encoding="utf-8")
    tmp.replace(path)


def _format_breach_line(breach: Breach) -> str:
    return (
        f"[{breach.severity.upper()}] {breach.key}: actual={breach.actual} "
        f"threshold={breach.threshold} - {breach.message}"
    )


def _dispatch_terminal(breaches: list[Breach], report: dict[str, Any]) -> None:
    scope = report.get("scope", "")
    print(f"Backlog health breach in {scope}:", file=sys.stderr)
    for b in breaches:
        print(f"  {_format_breach_line(b)}", file=sys.stderr)


def _dispatch_log_only(
    breaches: list[Breach], report: dict[str, Any], alert_log_path: Path
) -> None:
    alert_log_path.parent.mkdir(parents=True, exist_ok=True)
    timestamp = datetime.now(timezone.utc).isoformat()
    scope = report.get("scope", "")
    with alert_log_path.open("a", encoding="utf-8") as f:
        f.write(f"{timestamp}\t{scope}\n")
        for b in breaches:
            f.write(f"\t{_format_breach_line(b)}\n")


def _dispatch_webhook(
    breaches: list[Breach], report: dict[str, Any], webhook_url: str
) -> bool:
    """POST a JSON payload to ``webhook_url``. Returns True on 2xx."""
    payload = {
        "scope": report.get("scope", ""),
        "breaches": [b.to_jsonable() for b in breaches],
        "report_summary": report.get("totals", {}),
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        webhook_url,
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return 200 <= resp.status < 300
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError) as exc:
        print(
            f"health_monitor: webhook dispatch failed: {exc}",
            file=sys.stderr,
        )
        return False


def _dispatch_discord(
    breaches: list[Breach], report: dict[str, Any], channel_or_url: str
) -> bool:
    """Discord dispatch surface.

    Two flavors are supported:
      - URL form (starts with ``https://``): treat as a Discord webhook URL
        and POST a Discord-shaped payload to it.
      - Anything else: no shell helper is wired in the repo today; emit a
        stderr notice and fall through. The webhook surface is the
        recommended path for cron-style use.
    """
    if channel_or_url and channel_or_url.startswith("https://"):
        # Discord webhook payload shape: {"content": "..."}
        scope = report.get("scope", "")
        lines = [f"**Backlog health breach** ({scope})"]
        for b in breaches:
            lines.append(f"- {_format_breach_line(b)}")
        payload = {"content": "\n".join(lines)}
        data = json.dumps(payload).encode("utf-8")
        req = urllib.request.Request(
            channel_or_url,
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return 200 <= resp.status < 300
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError) as exc:
            print(
                f"health_monitor: discord webhook failed: {exc}",
                file=sys.stderr,
            )
            return False
    # Non-URL channel reference: no shell helper available.
    print(
        f"health_monitor: discord surface configured with channel "
        f"{channel_or_url!r} but no helper is wired; configure a Discord "
        f"webhook URL or use the webhook surface instead. Falling through.",
        file=sys.stderr,
    )
    return False


def dispatch_notifications(
    report: dict[str, Any],
    breaches: list[Breach],
    config: Optional[dict[str, Any]] = None,
    throttle_path: Optional[Path] = None,
    alert_log_path: Optional[Path] = None,
) -> None:
    """Dispatch breach notifications to configured surfaces.

    Throttle semantics: breaches with severity != "alert" are suppressed
    if the same breach key was notified within ``throttle_minutes``. Alert
    severity always fires. The throttle state is keyed on breach.key, not
    on the surface, so two surfaces share a throttle window.

    Surface ordering is taken from config. If a surface raises or returns
    falsy, the next surface in the list is tried. ``log_only`` always
    succeeds. This means even if discord/webhook are misconfigured, the
    breach is recorded somewhere.
    """
    if not breaches:
        return
    if config is None:
        config = load_config()
    if not config.get("enabled", True):
        return

    notif_cfg = config.get("notifications", {}) or {}
    surfaces: Iterable[str] = notif_cfg.get("surfaces", ["terminal"]) or ["terminal"]
    throttle_minutes = int(notif_cfg.get("throttle_minutes", 60) or 0)
    throttle_path = throttle_path or _default_throttle_path()
    alert_log_path = alert_log_path or _default_alert_log_path()

    # Throttle: drop info/warn breaches that fired recently. Alerts always go.
    now = datetime.now(timezone.utc)
    throttle_state = _read_throttle_state(throttle_path)
    fresh: list[Breach] = []
    for b in breaches:
        if b.severity == "alert":
            fresh.append(b)
            continue
        if throttle_minutes <= 0:
            fresh.append(b)
            continue
        last_str = throttle_state.get(b.key)
        if not last_str:
            fresh.append(b)
            continue
        try:
            last_dt = datetime.fromisoformat(last_str)
        except ValueError:
            fresh.append(b)
            continue
        if last_dt.tzinfo is None:
            last_dt = last_dt.replace(tzinfo=timezone.utc)
        if (now - last_dt) >= timedelta(minutes=throttle_minutes):
            fresh.append(b)
        # else: suppressed
    if not fresh:
        return

    # Dispatch in surface order; log_only always last-resort.
    dispatched_any = False
    for surface in surfaces:
        try:
            if surface == "terminal":
                _dispatch_terminal(fresh, report)
                dispatched_any = True
            elif surface == "log_only":
                _dispatch_log_only(fresh, report, alert_log_path)
                dispatched_any = True
            elif surface == "webhook":
                url = notif_cfg.get("webhook_url")
                if not url:
                    print(
                        "health_monitor: webhook surface requested but webhook_url is unset",
                        file=sys.stderr,
                    )
                    continue
                if _dispatch_webhook(fresh, report, url):
                    dispatched_any = True
            elif surface == "discord":
                channel = notif_cfg.get("discord_channel")
                if not channel:
                    print(
                        "health_monitor: discord surface requested but discord_channel is unset",
                        file=sys.stderr,
                    )
                    continue
                if _dispatch_discord(fresh, report, channel):
                    dispatched_any = True
            else:
                print(
                    f"health_monitor: unknown notification surface {surface!r}",
                    file=sys.stderr,
                )
        except Exception as exc:  # surface dispatch must never bubble
            print(
                f"health_monitor: {surface} dispatch raised {type(exc).__name__}: {exc}",
                file=sys.stderr,
            )

    if not dispatched_any:
        # Belt-and-braces: ensure something was recorded.
        try:
            _dispatch_log_only(fresh, report, alert_log_path)
        except Exception as exc:
            print(
                f"health_monitor: log_only fallback raised {type(exc).__name__}: {exc}",
                file=sys.stderr,
            )

    # Update throttle state only for non-alert dispatches. Alerts bypass
    # throttle on read, and stamping them here would suppress subsequent
    # warn-severity notifications of the same key inside the throttle
    # window (see code-review finding #7).
    for b in fresh:
        if b.severity == "alert":
            continue
        throttle_state[b.key] = now.isoformat()
    try:
        _write_throttle_state(throttle_path, throttle_state)
    except OSError as exc:
        print(
            f"health_monitor: cannot persist throttle state to {throttle_path}: {exc}",
            file=sys.stderr,
        )


# ---------------------------------------------------------------------------
# History log
# ---------------------------------------------------------------------------


def _default_history_path() -> Path:
    return _paths.state_dir() / "health-history.jsonl"


def append_history(
    report: dict[str, Any],
    breaches: list[Breach],
    history_path: Optional[Path] = None,
    retain_days: Optional[int] = None,
) -> None:
    """Append a JSONL line summarising this check; prune entries older than retain_days.

    Both healthy and breach states are logged - the trend verb consumes
    the full series. Pruning runs on every append: read the file, drop
    entries older than ``retain_days``, write back. For 90-day retention
    at 1 check per hour that's ~2160 entries - small enough that
    read-modify-write is fine.

    Malformed existing lines are dropped silently (a stderr warning is
    emitted) rather than raising; the worst case is a noisy log file
    that self-heals on the next append.
    """
    if history_path is None:
        history_path = _default_history_path()
    if retain_days is None:
        retain_days = DEFAULT_CONFIG["history"]["retain_days"]

    history_path.parent.mkdir(parents=True, exist_ok=True)
    cutoff = datetime.now(timezone.utc) - timedelta(days=retain_days)
    surviving: list[dict[str, Any]] = []
    abort_overwrite = False
    if history_path.exists():
        try:
            text = history_path.read_text(encoding="utf-8")
        except OSError as exc:
            # Existing history is unreadable. Do NOT overwrite - that would
            # silently destroy whatever the file holds. Surface loudly and
            # skip the append; the next successful run will retry.
            print(
                f"health_monitor: existing history at {history_path} unreadable "
                f"({exc}); skipping append to avoid clobbering forensic data. "
                f"Fix permissions and rerun.",
                file=sys.stderr,
            )
            return
        malformed_count = 0
        for line in text.splitlines():
            stripped = line.strip()
            if not stripped:
                continue
            try:
                entry = json.loads(stripped)
            except ValueError:
                malformed_count += 1
                continue
            ts_str = entry.get("timestamp")
            try:
                ts = datetime.fromisoformat(str(ts_str).replace("Z", "+00:00"))
            except (TypeError, ValueError):
                # malformed timestamp - keep for forensic value if recent-looking,
                # but the safer default is to drop.
                malformed_count += 1
                continue
            if ts.tzinfo is None:
                ts = ts.replace(tzinfo=timezone.utc)
            if ts >= cutoff:
                surviving.append(entry)
        if malformed_count:
            print(
                f"health_monitor: dropped {malformed_count} malformed line(s) "
                f"from {history_path}",
                file=sys.stderr,
            )
            # If the entire file was unparseable, preserve the corrupt
            # bytes for forensics before overwriting (best effort).
            if surviving == [] and text.strip():
                corrupt_path = history_path.with_name(
                    f"{history_path.name}.corrupt.{int(datetime.now(timezone.utc).timestamp())}"
                )
                try:
                    corrupt_path.write_text(text, encoding="utf-8")
                    print(
                        f"health_monitor: preserved unparseable history at "
                        f"{corrupt_path}",
                        file=sys.stderr,
                    )
                except OSError as exc:
                    print(
                        f"health_monitor: could not preserve corrupt history: {exc}",
                        file=sys.stderr,
                    )

    new_entry = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "scope": report.get("scope", ""),
        "report": report,
        "breaches": [b.to_jsonable() for b in breaches],
    }
    surviving.append(new_entry)

    # Atomic rewrite: write to a pid-disambiguated tmp, then rename.
    # Pid disambiguation prevents two concurrent appends from both
    # writing to the same .tmp path and clobbering each other mid-rename.
    tmp = history_path.with_name(
        f"{history_path.name}.{os.getpid()}.tmp"
    )
    try:
        tmp.write_text(
            "\n".join(json.dumps(e) for e in surviving) + "\n",
            encoding="utf-8",
        )
        tmp.replace(history_path)
    except OSError as exc:
        # Best-effort cleanup of the tmp file; surface the failure so
        # the operator knows the append did not land.
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        print(
            f"health_monitor: could not write history to {history_path}: {exc}",
            file=sys.stderr,
        )


# ---------------------------------------------------------------------------
# Trend summary (consumed by `fno backlog triage trend`)
# ---------------------------------------------------------------------------


def read_history(
    history_path: Optional[Path] = None,
    days: int = 7,
) -> list[dict[str, Any]]:
    """Return history entries from the last ``days`` days, newest first.

    Malformed lines are skipped. Missing history file yields an empty list.
    """
    if history_path is None:
        history_path = _default_history_path()
    if not history_path.exists():
        return []
    cutoff = datetime.now(timezone.utc) - timedelta(days=days)
    out: list[dict[str, Any]] = []
    try:
        text = history_path.read_text(encoding="utf-8")
    except OSError:
        return []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        try:
            entry = json.loads(stripped)
        except ValueError:
            continue
        ts_str = entry.get("timestamp")
        try:
            ts = datetime.fromisoformat(str(ts_str).replace("Z", "+00:00"))
        except (TypeError, ValueError):
            continue
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=timezone.utc)
        if ts >= cutoff:
            out.append(entry)
    out.sort(key=lambda e: e.get("timestamp", ""), reverse=True)
    return out


def summarize_trend(entries: list[dict[str, Any]]) -> dict[str, Any]:
    """Compute first vs latest deltas for the four headline metrics.

    Returns a dict keyed by metric name with ``first``, ``latest``,
    ``delta``, and ``percent_change`` fields. Empty input yields an empty
    summary dict.
    """
    if not entries:
        return {}

    # entries are newest-first; reverse for chronological order.
    chrono = list(reversed(entries))
    first = chrono[0].get("report", {})
    latest = chrono[-1].get("report", {})

    def _count_or_len(report: dict[str, Any], key: str) -> int:
        v = report.get(key)
        if isinstance(v, list):
            return len(v)
        try:
            return int(v or 0)
        except (TypeError, ValueError):
            return 0

    keys = [
        ("idea_pile_depth", "idea_pile_depth"),
        ("stale_ready_nodes", "stale_ready_nodes"),
        ("failure_prone_nodes", "failure_prone_nodes"),
        ("collisions", "collisions"),
        ("project_cwd_mismatch", "project_cwd_mismatch"),
    ]
    summary: dict[str, Any] = {}
    for label, report_key in keys:
        f = _count_or_len(first, report_key)
        latest_val = _count_or_len(latest, report_key)
        delta = latest_val - f
        if f == 0:
            percent_change: Optional[float] = None  # avoid div-by-zero noise
        else:
            percent_change = round((delta / f) * 100, 1)
        summary[label] = {
            "first": f,
            "latest": latest_val,
            "delta": delta,
            "percent_change": percent_change,
        }
    return summary

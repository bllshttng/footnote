"""Machine load beside a trial: the highest 1-minute load sampled inside a time window.

logs/load.jsonl in the run workspace holds one sample a minute. A window with no
sample returns None (unknown), never zero.
"""
import json
from datetime import datetime, timezone
from paths import LOGS

LOG = LOGS / "load.jsonl"
HIGH = 50.0


def ts(s: str) -> float:
    d = datetime.fromisoformat(s.replace("Z", "+00:00"))
    return (d if d.tzinfo else d.replace(tzinfo=timezone.utc)).timestamp()


def samples() -> list:
    if not LOG.is_file():
        return []
    return [(ts(r["at"]), r["load1"]) for r in map(json.loads, LOG.read_text().splitlines())]


def max_load(start: float, end: float, rows: list) -> float | None:
    inside = [v for t, v in rows if start <= t <= end]
    return max(inside) if inside else None


if __name__ == "__main__":
    rows = [(0.0, 1.0), (60.0, 80.0), (120.0, 3.0)]
    assert max_load(30, 90, rows) == 80.0 and max_load(200, 300, rows) is None
    assert ts("2026-09-30T16:40:06Z") == ts("2026-09-30T16:40:06+00:00") == ts("2026-09-30T16:40:06")
    print("ok", len(samples()), "samples")

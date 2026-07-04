"""fno scoreboard - read-only telemetry fold over the ledger + events.

Wave 5 of epic x-f063. Writes no state, ever. Folds what exists:
ledger.json (stop-cause, spend, coverage) plus events.jsonl (human_touch,
for autonomy) and the graph (reverted/caused_by, for survival). The Wave 4
signals degrade to n/a until Wave 4 ships them.
"""

from fno.scoreboard.fold import BrokenLedger, build_scoreboard

__all__ = ["BrokenLedger", "build_scoreboard"]

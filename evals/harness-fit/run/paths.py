"""Where the study lives. The code is in this repo; runs, logs and secrets stay in the
run workspace outside it (HARNESS_FIT_WS, default ~/evals-workspace/harness-fit)."""
import os
from pathlib import Path

HERE = Path(__file__).resolve().parent
STUDY = HERE.parent
REPO = STUDY.parents[1]
MANIFEST = STUDY / "manifest.json"
BANK = STUDY / "bank"
WS = Path(os.environ.get("HARNESS_FIT_WS", Path.home() / "evals-workspace" / "harness-fit")).expanduser()
RUNS = WS / "runs"
LOGS = WS / "logs"
HISTORY = Path.home() / ".fno" / "history" / "evals-history.jsonl"

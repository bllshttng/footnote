#!/usr/bin/env python3
"""One compact status read of both runs, from the scorers and the driver logs."""
import json
import subprocess
import sys
import time

from paths import HERE, LOGS

print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
      "| docker", "ok" if subprocess.run(["docker", "info"], capture_output=True).returncode == 0 else "DOWN",
      "| load", subprocess.run(["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True).stdout.strip())
for script, key, fields in (("score_run0.py", "arms", ("trials", "graded", "passed", "rate", "excluded", "usd")),
                            ("score_run1.py", "lanes", ("attempts", "scored", "stalls", "accepted", "excluded", "usd"))):
    out = subprocess.run([sys.executable, str(HERE / script)], capture_output=True, text=True).stdout
    for name, a in json.loads(out)[key].items():
        print(f"  {script[6:10]} {name}:", " ".join(f"{f} {a[f]}" for f in fields))
for name in ("run0-driver.jsonl", "run1-driver.jsonl"):
    path = LOGS / name
    lines = path.read_text().splitlines() if path.exists() else ["(none)"]
    print(" ", name, "last:", lines[-1][:200])

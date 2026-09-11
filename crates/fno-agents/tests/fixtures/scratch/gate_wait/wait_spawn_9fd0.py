import os, subprocess, time
DEADLINE = time.time() + 2400
while time.time() < DEADLINE:
    load1 = os.getloadavg()[0]
    if load1 < 88:
        r = subprocess.run(["python3", "~/.claude/jobs/JOBID/tmp/spawn_9fd0.py"],
                           capture_output=True, text=True, timeout=900)
        out = (r.stdout or "") + (r.stderr or "")
        print(f"attempt at load {load1:.1f}:\n{out.strip()[-500:]}")
        if "rc=0" in out:
            print("SPAWNED")
            raise SystemExit(0)
        if "spawn-gate" not in out:
            print("FAILED for a reason other than the load gate; stopping")
            raise SystemExit(1)
    time.sleep(120)
print(f"deadline reached, still refused; last 1-min load {os.getloadavg()[0]:.1f}")
raise SystemExit(2)

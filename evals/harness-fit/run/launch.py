#!/usr/bin/env python3
"""Run 0 launcher: one Harbor job per arm over Terminal-Bench 2.

  launch.py smoke <arm>     one-task smoke, job harness-fit-smoke2-<arm>
  launch.py full <arm>      89-task pass, job harness-fit-run0-<arm>; refuses past the ceiling
  launch.py continue <arm>  finish a killed job: only trials with no result run
  launch.py resume <arm>    rerun a job's RuntimeError trials (Docker down)
  launch.py spend           list-price USD of every job

Run through `uv run --with pyyaml`. The key comes from ~/.fno/.env (ZAI_API_KEY).
Resolved configs are 0600 and stay in the run workspace.
"""
import json
import os
import stat
import subprocess
import sys
from pathlib import Path

import yaml

from paths import MANIFEST, RUNS, WS

HARBOR = ["uvx", "--from", "harbor==0.23.0", "harbor"]
SMOKE_TASK = "terminal-bench/adaptive-rejection-sampler"
MODEL = "glm-5.3-flash"
ZAI_ANTHROPIC = "https://api.z.ai/api/anthropic"
ZAI_OPENAI = "https://api.z.ai/api/coding/paas/v4"
ARMS = ["claude-code", "opencode", "pi", "terminus-2", "zcode"]
PROJECTION_FACTOR = 3
TASKS = 89


def concurrency() -> int:
    """Amendment 6: min(4, half the physical cores)."""
    cores = int(subprocess.run(["sysctl", "-n", "hw.physicalcpu"], capture_output=True, text=True).stdout or 8)
    return max(1, min(4, cores // 2))


def zai_key() -> str:
    for line in (Path.home() / ".fno/.env").read_text().splitlines():
        if line.startswith("ZAI_API_KEY="):
            return line.split("=", 1)[1].strip().strip('"')
    raise SystemExit("ZAI_API_KEY missing from ~/.fno/.env")


def agent(arm: str, key: str) -> tuple[dict, list[str]]:
    """Return (agent config, container env rows) for one arm."""
    if arm == "claude-code":
        env = [f"ANTHROPIC_BASE_URL={ZAI_ANTHROPIC}", f"ANTHROPIC_AUTH_TOKEN={key}", "API_TIMEOUT_MS=600000"]
        env += [f"{v}={MODEL}" for v in ("ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL",
                                         "ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                                         "ANTHROPIC_DEFAULT_FABLE_MODEL")]
        return ({"import_path": "harbor.agents.installed.claude_code:ClaudeCode", "model_name": MODEL,
                 "kwargs": {"reasoning_effort": "high"}}, env)
    if arm == "opencode":
        return ({"import_path": "harbor.agents.installed.opencode:OpenCode",
                 "model_name": f"zai-coding-plan/{MODEL}",
                 "kwargs": {"opencode_config": {"provider": {"zai-coding-plan": {"options": {
                     "baseURL": ZAI_OPENAI, "apiKey": key}}}}}},
                [f"ZAI_CODING_PLAN_API_KEY={key}"])
    if arm == "pi":
        return ({"import_path": "harbor.agents.installed.pi:Pi", "model_name": f"openai/{MODEL}",
                 "kwargs": {"model_api": "openai-completions", "thinking": "high"}}, [])
    if arm == "terminus-2":
        return ({"import_path": "harbor.agents.terminus_2:Terminus2", "model_name": f"openai/{MODEL}",
                 "kwargs": {"api_base": ZAI_OPENAI}}, [])
    if arm == "zcode":
        # The adapter stays out of the repo; copy zcode_agent.py and zcode-home into the workspace.
        return ({"import_path": "zcode_agent:ZCode", "model_name": MODEL}, [])
    raise SystemExit(f"unknown arm {arm!r}; arms: {ARMS}")


def host_env(key: str, arm: str) -> dict:
    env = dict(os.environ)
    env.update({"ZAI_API_KEY": key, "ZAI_BASE_URL": ZAI_OPENAI, "OPENAI_API_KEY": key, "PYTHONPATH": str(WS)})
    if arm == "pi":
        # Harbor's zai provider has no base-URL env; the openai provider does.
        env["OPENAI_BASE_URL"] = ZAI_OPENAI
    if arm == "zcode":
        env["ZCODE_CREDENTIAL_SECRET"] = (f"zcode-credential-fallback:darwin:{Path.home()}:"
                                          f"{os.environ.get('USER', '')}")
    return env


def job_cost(result: Path, prices: dict) -> float:
    s = json.loads(result.read_text()).get("stats", {})
    p = prices[MODEL]
    total_in = s.get("n_input_tokens") or 0
    cache = s.get("n_cache_tokens") or 0
    out = s.get("n_output_tokens") or 0
    return (max(total_in - cache, 0) * p["input_per_m"] + cache * p["cache_read_per_m"]
            + out * p["output_per_m"]) / 1e6


def spend(prices: dict) -> dict:
    return {r.parent.name: round(job_cost(r, prices), 4) for r in sorted(RUNS.glob("*/result.json"))}


def pilot_spend(manifest: dict) -> float:
    """Amendment 6: the arm64 pilot's dollars count against the ceiling."""
    return manifest["amendment_6"]["pilot_spend_usd"]


def unmask(config: Path, key: str) -> int:
    """Harbor saves secrets masked (`a12e****Q47`) and a resume sends them as is: HTTP 401.
    Put the real key back wherever its masked form sits. Returns how many it restored."""
    n = 0

    def fix(v):
        nonlocal n
        if isinstance(v, dict):
            return {k: fix(x) for k, x in v.items()}
        if isinstance(v, list):
            return [fix(x) for x in v]
        if isinstance(v, str) and "****" in v:
            head, tail = v.split("****", 1)
            lead = head.rsplit("=", 1)[0] + "=" if "=" in head else ""  # `NAME=a12e****Q47` rows
            head = head[len(lead):]
            if key[:len(head)] == head and (not tail or key[-len(tail):] == tail):
                n += 1
                return lead + key
        return v

    fixed = fix(json.loads(config.read_text()))
    if n:
        config.write_text(json.dumps(fixed, indent=4))
        config.chmod(stat.S_IRUSR | stat.S_IWUSR)
    return n


def run(mode: str, arm: str) -> int:
    manifest = json.loads(MANIFEST.read_text())
    prices, ceiling = manifest["prices"], manifest["ceiling_usd"]
    key = zai_key()
    agent_cfg, env_rows = agent(arm, key)
    if mode == "full":
        spent = sum(spend(prices).values()) + pilot_spend(manifest)
        smoke = RUNS / f"harness-fit-smoke2-{arm}" / "result.json"
        # Harbor undercounts some arms (opencode drops reasoning tokens), so the
        # projection never uses less than 0.25 USD per task.
        per_task = max(job_cost(smoke, prices) if smoke.exists() else 0.5, 0.25)
        projection = PROJECTION_FACTOR * per_task * TASKS
        print(f"ceiling check: spent {spent:.2f} + projection {projection:.2f} vs ceiling {ceiling:.2f}")
        if spent + projection >= ceiling:
            print(f"STOP: ceiling {ceiling} would be passed; {arm} not started")
            return 3
    job = f"harness-fit-{'smoke2' if mode == 'smoke' else 'run0'}-{arm}"
    dataset = {"name": "terminal-bench/terminal-bench-2"}
    if mode == "smoke":
        dataset["task_names"] = [SMOKE_TASK]
    cfg = {
        "job_name": job, "jobs_dir": str(RUNS), "n_attempts": 1,
        "n_concurrent_trials": 1 if mode == "smoke" else concurrency(),
        "timeout_multiplier": 1.0, "agent_setup_timeout_multiplier": 3.0,
        "environment": {"type": "docker", "force_build": False, "delete": True, "env": env_rows},
        "agents": [agent_cfg], "datasets": [dataset],
    }
    out = WS / "resolved" / f"{job}.yaml"
    out.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    out.write_text(yaml.safe_dump(cfg, sort_keys=False))
    out.chmod(stat.S_IRUSR | stat.S_IWUSR)
    return subprocess.call([*HARBOR, "run", "--config", str(out)], env=host_env(key, arm), cwd=WS)


def main(argv: list[str]) -> int:
    if argv[:1] == ["spend"]:
        rows = spend(json.loads(MANIFEST.read_text())["prices"])
        print(json.dumps({"jobs": rows, "total_usd": round(sum(rows.values()), 4)}, indent=1))
        return 0
    if len(argv) == 2 and argv[0] in ("resume", "continue"):
        key = zai_key()
        job = RUNS / f"harness-fit-run0-{argv[1]}"
        print(f"unmask: restored {unmask(job / 'config.json', key)} secret(s)")
        # continue: Harbor reruns only trials with no result or a CancelledError; every result stands.
        flt = ["-f", "RuntimeError"] if argv[0] == "resume" else []
        return subprocess.call([*HARBOR, "jobs", "resume", "-p", str(job), *flt], env=host_env(key, argv[1]), cwd=WS)
    if len(argv) != 2 or argv[0] not in ("smoke", "full"):
        print(__doc__)
        return 2
    return run(argv[0], argv[1])


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

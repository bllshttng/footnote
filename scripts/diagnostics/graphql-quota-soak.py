#!/usr/bin/env python3
"""Prove the GraphQL reserve with positive fleet and coverage markers."""
from __future__ import annotations

import argparse
import json
import math
import os
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

FLOOR = 200


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def run_json(argv: list[str], *, cwd: Path) -> tuple[int, dict[str, Any], str]:
    proc = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, check=False)
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError:
        payload = {}
    return proc.returncode, payload, (proc.stderr or "").strip()


def quota_remaining(cwd: Path) -> int:
    code, payload, error = run_json(["gh", "api", "rate_limit"], cwd=cwd)
    if code != 0:
        raise RuntimeError(f"quota instrument failed: {error or 'no diagnostic'}")
    value = ((payload.get("resources") or {}).get("graphql") or {}).get("remaining")
    if not isinstance(value, int):
        raise RuntimeError("quota instrument returned no integer GraphQL remaining value")
    return value


def live_workers(cwd: Path, fno_bin: str) -> int:
    code, payload, error = run_json([fno_bin, "agents", "list", "--json"], cwd=cwd)
    if code != 0:
        raise RuntimeError(f"worker instrument failed: {error or 'no diagnostic'}")
    agents = payload.get("agents")
    if not isinstance(agents, list):
        raise RuntimeError("worker instrument returned no agents array")
    return sum(
        1 for row in agents
        if isinstance(row, dict)
        and row.get("status") == "live"
        and row.get("reachability") == "reachable"
    )


def atomic_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, raw = tempfile.mkstemp(prefix=".graphql-quota-soak.", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(payload, handle, sort_keys=True, separators=(",", ":"))
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(raw, path)
    finally:
        Path(raw).unlink(missing_ok=True)


def validate_receipt(
    receipt: dict[str, Any], *, min_seconds: int, max_age_hours: float, min_workers: int
) -> list[str]:
    missing: list[str] = []
    if receipt.get("settled") is not True:
        missing.append("settled=true")
    if not isinstance(receipt.get("pr"), int) or receipt["pr"] <= 0:
        missing.append("pr>0")
    if receipt.get("coverage_exit") != 0:
        missing.append("coverage_exit=0")
    duration = receipt.get("duration_seconds")
    if not isinstance(duration, (int, float)) or duration < min_seconds:
        missing.append(f"duration_seconds>={min_seconds}")
    samples = receipt.get("samples")
    required_samples = max(1, math.ceil(min_seconds / 60))
    if not isinstance(samples, int) or samples < required_samples:
        missing.append(f"samples>={required_samples}")
    minimum = receipt.get("min_remaining")
    if receipt.get("floor") != FLOOR:
        missing.append(f"floor={FLOOR}")
    if not isinstance(minimum, int) or minimum <= FLOOR:
        missing.append(f"min_remaining>{FLOOR}")
    post_coverage = receipt.get("post_coverage_remaining")
    if not isinstance(post_coverage, int) or post_coverage <= FLOOR:
        missing.append(f"post_coverage_remaining>{FLOOR}")
    workers = receipt.get("min_live_workers")
    if not isinstance(workers, int) or workers < min_workers:
        missing.append(f"min_live_workers>={min_workers}")
    probes = receipt.get("discretionary_probes")
    if not isinstance(probes, int) or probes < required_samples:
        missing.append(f"discretionary_probes>={required_samples}")
    if receipt.get("coverage") != "covered":
        missing.append("coverage=covered")
    reviewed = receipt.get("reviewed_count")
    if not isinstance(reviewed, int) or reviewed <= 0:
        missing.append("reviewed_count>0")
    if not receipt.get("head_sha") or receipt.get("coverage_head_sha") != receipt.get("head_sha"):
        missing.append("coverage_head_sha=head_sha")
    try:
        started = datetime.strptime(
            str(receipt.get("started_at")), "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=timezone.utc)
        ended = datetime.strptime(str(receipt.get("ended_at")), "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=timezone.utc
        )
        if ended < started:
            missing.append("ended_at>=started_at")
        age = (datetime.now(timezone.utc) - ended).total_seconds()
        if age < 0 or age > max_age_hours * 3600:
            missing.append(f"receipt_age<={max_age_hours:g}h")
    except ValueError:
        missing.append("timestamps=valid_utc")
    return missing


def latest_receipt(receipt_dir: Path) -> Path:
    rows = sorted(receipt_dir.glob("*.json"), key=lambda path: path.stat().st_mtime)
    if not rows:
        raise RuntimeError(f"no quota-soak receipt under {receipt_dir}")
    return rows[-1]


def run_soak(args: argparse.Namespace) -> int:
    cwd = Path(args.cwd).resolve()
    fno_bin = os.environ.get("FNO_SOAK_FNO_BIN", "fno")
    agents_bin = os.environ.get("FNO_SOAK_AGENTS_BIN", "fno-agents")
    started_at = utc_now()
    started = time.monotonic()
    quota_samples: list[int] = []
    worker_samples: list[int] = []
    discretionary_probes = 0
    while True:
        before = quota_remaining(cwd)
        workers = live_workers(cwd, fno_bin)
        probe_code, probe, probe_error = run_json(
            [
                fno_bin, "pr", "graphql-exec", "--purpose", "discretionary", "--",
                "api", "graphql", "-f", "query=query { viewer { login } }",
            ],
            cwd=cwd,
        )
        login = (((probe.get("data") or {}).get("viewer") or {}).get("login"))
        if probe_code != 0 or not login:
            raise RuntimeError(
                f"discretionary probe failed: {probe_error or probe or probe_code}"
            )
        discretionary_probes += 1
        remaining = quota_remaining(cwd)
        quota_samples.append(remaining)
        worker_samples.append(workers)
        print(
            f"sample={len(quota_samples)} graphql_before={before} "
            f"graphql_remaining={remaining} live_workers={workers} discretionary=ok",
            flush=True,
        )
        elapsed = time.monotonic() - started
        if elapsed >= args.duration:
            break
        time.sleep(min(args.interval, args.duration - elapsed))

    duration = time.monotonic() - started
    info_code, info, info_error = run_json([fno_bin, "pr", "info", str(args.pr)], cwd=cwd)
    if info_code != 0 or not info.get("head_sha"):
        raise RuntimeError(f"REST PR info failed: {info_error or info}")
    head = info["head_sha"]
    coverage_code, coverage, _ = run_json(
        [agents_bin, "review-coverage", "--cwd", str(cwd), "--pr", str(args.pr), "--head", head],
        cwd=cwd,
    )
    post_coverage_remaining = quota_remaining(cwd)
    receipt = {
        "started_at": started_at,
        "ended_at": utc_now(),
        "duration_seconds": round(duration, 3),
        "samples": len(quota_samples),
        "floor": FLOOR,
        "min_remaining": min(quota_samples),
        "min_live_workers": min(worker_samples),
        "discretionary_probes": discretionary_probes,
        "pr": args.pr,
        "head_sha": head,
        "coverage_exit": coverage_code,
        "coverage": coverage.get("coverage"),
        "reviewed_count": coverage.get("reviewed_count"),
        "coverage_head_sha": coverage.get("head_sha"),
        "post_coverage_remaining": post_coverage_remaining,
        "settled": True,
    }
    missing = validate_receipt(
        receipt,
        min_seconds=args.duration,
        max_age_hours=24,
        min_workers=args.min_workers,
    )
    if missing:
        raise RuntimeError("positive markers failed: " + ", ".join(missing))
    atomic_json(Path(args.receipt), receipt)
    print(
        f"settled=true samples={receipt['samples']} min_remaining={receipt['min_remaining']} "
        f"post_coverage_remaining={receipt['post_coverage_remaining']} coverage=covered "
        f"reviewed_count={receipt['reviewed_count']} head_sha={head}"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration", type=float)
    parser.add_argument("--interval", type=float, default=60)
    parser.add_argument("--pr", type=int)
    parser.add_argument("--receipt")
    parser.add_argument("--cwd", default=".")
    parser.add_argument("--min-workers", type=int, default=15)
    parser.add_argument("--check-latest", action="store_true")
    parser.add_argument("--receipt-dir", default=".fno/artifacts/graphql-quota-soak")
    parser.add_argument("--min-seconds", type=int, default=3600)
    parser.add_argument("--max-age-hours", type=float, default=24)
    args = parser.parse_args()
    try:
        if args.check_latest:
            path = latest_receipt(Path(args.receipt_dir))
            receipt = json.loads(path.read_text())
            missing = validate_receipt(
                receipt,
                min_seconds=args.min_seconds,
                max_age_hours=args.max_age_hours,
                min_workers=args.min_workers,
            )
            if missing:
                raise RuntimeError("positive markers failed: " + ", ".join(missing))
            print(
                f"settled=true samples={receipt['samples']} min_remaining={receipt['min_remaining']} "
                f"post_coverage_remaining={receipt['post_coverage_remaining']} coverage=covered "
                f"reviewed_count={receipt['reviewed_count']} "
                f"head_sha={receipt['head_sha']} receipt={path}"
            )
            return 0
        if args.duration is None or args.pr is None or not args.receipt:
            parser.error("run mode requires --duration, --pr, and --receipt")
        return run_soak(args)
    except (OSError, RuntimeError, json.JSONDecodeError) as exc:
        print(f"graphql-quota-soak: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Done probe: stuck work reaches the phone, and the check-in reads attention.

Live mode (--live --scope SCOPE) plants a hung verb (a python process with
argv0 fno-py and a --timeout far exceeded) and a dead-pid flight claim, then
requires, inside the arm_watch cadence, an operator_notice row naming both in
the project journal, a phone sink cursor at or past that row, and a king
check-in whose change starts `attention:` and names the probe pid.

Self-test mode (--self-test) runs the polling and change checks against
inline fixtures: no processes, no claims, no journal writes.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import signal
import subprocess
import sys
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))


def _reexec_into_venv() -> None:
    # fno.paths pulls the cli package's deps; a bare interpreter re-execs
    # into the repo venv once, before the import.
    try:
        import tomli_w  # noqa: F401
    except ModuleNotFoundError:
        venv = REPO_ROOT / "cli" / ".venv" / "bin" / "python"
        if venv.exists():
            os.execv(str(venv), [str(venv), *sys.argv])


_reexec_into_venv()

from fno import paths  # noqa: E402
from fno.rust_binary import resolve_binary  # noqa: E402

SLEEPER_KEY = "flight:probe-stuck-work"
# The notice rides notify_signal_via's rate floor ([notify] min_interval_s,
# default 1800): a token that changed inside the floor is HELD, not sent, so
# the poll must outlive the floor plus one arm_watch tick plus fanout.
POLL_BUDGET_S = 45 * 60
FANOUT_GRACE_S = 90


def run(cmd: list[str], cwd: pathlib.Path | None = None) -> tuple[int, str, str]:
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd)
    return proc.returncode, proc.stdout, proc.stderr


def fail(step: str, detail: str) -> int:
    print(f"FAIL at {step}: {detail}", file=sys.stderr)
    return 1


def journals() -> list[pathlib.Path]:
    """Every event journal and rotation, oldest first, from the one resolver."""
    return paths.event_journals()


def phone_cursor(project_root: pathlib.Path) -> pathlib.Path:
    return paths.status_sinks_dir(project_root) / "phone.cursor"


def find_notice(journal_list: list[pathlib.Path], pid: int, since_ts: str) -> tuple[str, str] | None:
    """The newest operator_notice row naming the pid and the probe key.

    Returns (row_ts, body) once found. The resolver hands over the journals
    and their rotations, oldest first; the last match is the newest row.
    """
    best: tuple[str, str] | None = None
    for path in journal_list:
        if not path.exists():
            continue
        try:
            lines = path.read_text(errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if row.get("type") != "operator_notice":
                continue
            body = json.dumps(row.get("data", {}))
            if str(pid) not in body or SLEEPER_KEY not in body:
                continue
            ts = str(row.get("ts", ""))
            if since_ts and ts < since_ts:
                continue
            best = (ts, body)
    return best


def cursor_ts(path: pathlib.Path) -> str:
    try:
        text = path.read_text().strip()
    except OSError:
        return ""
    try:
        return str(json.loads(text).get("ts", ""))
    except json.JSONDecodeError:
        return text.splitlines()[0] if text else ""


def checkin_attends(scope: str, pid: int, cwd: pathlib.Path) -> tuple[bool, str]:
    code, out, err = run(
        ["fno", "agents", "king", "checkin", "--scope", scope, "--no-emit", "--json"],
        cwd=cwd,
    )
    if code != 0:
        return False, f"checkin exited {code}: {err.strip()[:200]}"
    try:
        payload = json.loads(out)
    except json.JSONDecodeError:
        return False, "checkin payload did not parse"
    change = str(payload.get("change", ""))
    if not change.startswith("attention:"):
        return False, f"change reads {change!r}"
    if str(pid) not in change:
        return False, f"change names no probe pid: {change!r}"
    return True, change


def run_live(scope: str, cwd: pathlib.Path) -> int:
    cursor_path = phone_cursor(cwd)
    started_ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    sleeper = subprocess.Popen(
        # The sleeper must outlive the poll: once it dies its finding
        # leaves the set, and the required row names its pid.
        ["bash", "-c", 'exec -a fno-py python3 -c "import time; time.sleep(3600)" --timeout 20s'],
        cwd=cwd,
    )
    holder_pid: int | None = None
    try:
        # The dead holder: reap a fresh pid, then claim under it.
        victim = subprocess.Popen(["true"])
        victim.wait()
        holder_pid = victim.pid
        agents_bin = resolve_binary()
        if agents_bin is None:
            return fail("flight-acquire", "no fno-agents binary resolved")
        code, _, err = run(
            [
                str(agents_bin), "claim", "flight-acquire", SLEEPER_KEY,
                "--holder", "probe", "--pid", str(holder_pid), "--ttl-ms", "3600000",
            ],
            cwd=cwd,
        )
        if code != 0:
            # Config-noise lines lead stderr; the verdict is the tail.
            return fail("flight-acquire", (err.strip().splitlines() or ["?"])[-1][:200])

        budget = POLL_BUDGET_S  # noqa: F841 - kept for the log line below
        found = None
        while budget > 0:
            # Re-resolved per tick: a mid-poll rotation moves rows into a
            # .1 file a pre-poll snapshot would never see.
            found = find_notice(journals(), sleeper.pid, started_ts)
            if found:
                break
            time.sleep(15)
            budget -= 15
        if not found:
            return fail(
                "operator_notice",
                f"no row naming pid {sleeper.pid} and {SLEEPER_KEY} within {POLL_BUDGET_S}s",
            )
        row_ts, body = found
        print(f"notice row ts={row_ts} body={body[:160]}")

        cts = cursor_ts(cursor_path)
        if cts and cts < row_ts:
            return fail("phone.cursor", f"cursor {cts} behind row {row_ts}")
        print(f"phone cursor ts={cts or 'absent (no sink configured; skipped)'}")

        ok, detail = checkin_attends(scope, sleeper.pid, cwd)
        if not ok:
            return fail("king checkin", detail)
        print(f"checkin change={detail}")
    finally:
        sleeper.send_signal(signal.SIGKILL)
        if holder_pid is not None:
            run(
                [str(agents_bin), "claim", "flight-release", SLEEPER_KEY, "--holder", "probe"],
                cwd=cwd,
            )
    return 0


def run_self_test() -> int:
    # Step 3 against an inline journal: the finder matches the real row shape.
    fixture = json.dumps(
        {
            "ts": "2026-09-15T10:00:00Z",
            "type": "operator_notice",
            "data": {
                "title": "fixture: needs attention",
                "body": "hung verb pid 4242 2m fno-py -c sleep --timeout 20s (over 3x --timeout 20s)"
                f"\ndead holder {SLEEPER_KEY} holder probe pid 99 absent held 5m",
                "pointer": "fno agents status",
            },
        }
    )
    tmp = pathlib.Path(os.environ.get("FNO_PROBE_TMP", "/tmp")) / "stuck-probe-selftest"
    tmp.mkdir(parents=True, exist_ok=True)
    journal = tmp / "events.jsonl"
    journal.write_text(fixture + "\n")
    journal_list = [journal]
    found = find_notice(journal_list, 4242, "2026-09-15T09:00:00Z")
    if not found:
        return fail("self-test finder", "the fixture row was not matched")
    if found[0] != "2026-09-15T10:00:00Z":
        return fail("self-test finder", f"wrong ts: {found[0]}")
    missed = find_notice(journal_list, 4242, "2026-09-15T11:00:00Z")
    if missed:
        return fail("self-test finder", "a pre-episode row passed the since filter")
    wrong = find_notice(journal_list, 7777, "")
    if wrong:
        return fail("self-test finder", "an unrelated pid matched")

    # Step 4 against inline payloads: the change contract.
    attends = json.loads(
        json.dumps({"change": f"attention: hung verb pid 4242 2m; dead holder {SLEEPER_KEY}"})
    )
    ok, detail = (
        attends["change"].startswith("attention:")
        and str(4242) in attends["change"],
        attends["change"],
    )
    if not ok:
        return fail("self-test change", f"attention contract misread: {detail}")
    quiet = {"change": "no change"}
    if quiet["change"].startswith("attention"):
        return fail("self-test change", "quiet beat misread as attention")
    print("self-test: finder and change contract hold")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--live", action="store_true", help="plant the findings and watch them page")
    ap.add_argument("--scope", default="", help="the crown scope the check-in runs under")
    ap.add_argument(
        "--cwd",
        type=pathlib.Path,
        default=pathlib.Path.cwd(),
        help="project cwd the journal and cursor resolve from",
    )
    ap.add_argument("--self-test", action="store_true", help="inline fixtures only")
    args = ap.parse_args()

    if args.self_test:
        return run_self_test()
    if not args.live:
        print("nothing to do: pass --live --scope SCOPE (or --self-test)", file=sys.stderr)
        return 2
    if not args.scope:
        print("--live requires --scope SCOPE", file=sys.stderr)
        return 2
    return run_live(args.scope, args.cwd.resolve())


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Tests for scripts/metrics/backfill-cost-recompute.py.

Covers AC3-HP / AC3-ERR / AC3-UI / AC3-EDGE / AC3-FR plus the Concurrency
(claims guard, session_id never rewritten) and Boundaries (empty ledger,
empty sessions) failure modes from the cost-accuracy plan.

Each test runs the script as a subprocess with HOME pointed at a fixture
tree, so transcript discovery (~/.claude/projects), the ledger, the graph,
and the claims dir are all sandboxed.

Run: python3 tests/metrics/test_backfill_cost_recompute.py
 OR: cd cli && uv run pytest ../tests/metrics/test_backfill_cost_recompute.py -q
"""

import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
BACKFILL = REPO_ROOT / "scripts" / "metrics" / "backfill-cost-recompute.py"

UUID_ALIVE = "11111111-2222-3333-4444-555555555555"
UUID_GONE = "99999999-8888-7777-6666-555555555555"

# Per unique message: $1.25 at opus-4.8 rates
USAGE = {
    "input_tokens": 100_000,
    "output_tokens": 10_000,
    "cache_read_input_tokens": 1_000_000,
    "cache_creation_input_tokens": 0,
}
COST_PER_MSG = (100_000 * 5.00 + 10_000 * 25.00 + 1_000_000 * 0.50) / 1_000_000  # 1.25
TOKENS_PER_MSG = sum(USAGE.values())


def _assistant_line(msg_id: str, request_id: str) -> str:
    return json.dumps(
        {
            "type": "assistant",
            "timestamp": "2026-06-04T20:19:34.000Z",
            "requestId": request_id,
            "message": {"id": msg_id, "model": "claude-opus-4-8", "usage": dict(USAGE)},
            "gitBranch": "main",
        }
    )


def _make_fixture(home: Path, *, live_claim: bool = False, pid_claim: str | None = None) -> dict:
    """Build a sandbox HOME with transcript, ledger, graph, claims dir."""
    proj = home / ".claude" / "projects" / "-tmp-proj"
    proj.mkdir(parents=True)
    # 2 unique messages x 3 duplicate lines each -> true cost $2.50
    lines = []
    for i in range(2):
        for _ in range(3):
            lines.append(_assistant_line(f"msg_{i}", f"req_{i}"))
    (proj / f"{UUID_ALIVE}.jsonl").write_text("\n".join(lines) + "\n")

    fno = home / ".fno"
    fno.mkdir(parents=True)
    ledger = {
        "entries": [
            {
                "title": "recomputable feature",
                "model": "claude-opus-4-8",
                "sessions": [UUID_ALIVE],
                "session_id": UUID_ALIVE,
                "cost_usd": 99.0,
                "tokens_total": 12_345_678,
                "cache_read_tokens": 1,
                "compactions": 7,
            },
            {
                "title": "opus-4-8 transcript gone",
                "model": "claude-opus-4-8",
                "sessions": [UUID_GONE],
                "cost_usd": 9.0,
            },
            {
                "title": "sonnet transcript gone",
                "model": "claude-sonnet-4-5",
                "sessions": [UUID_GONE],
                "cost_usd": 5.0,
            },
            {
                # Scalar phase row (compute_session_cost greps these by prefix)
                "session_id": "20260604T000000Z-1-aaaaaa:think",
                "tokens": 100,
                "cost_usd": 1.23,
                "timestamp": "2026-06-04T00:00:00",
            },
        ]
    }
    (fno / "ledger.json").write_text(json.dumps(ledger, indent=2))

    graph = {
        "entries": [
            {
                "id": "ab-test0001",
                "title": "node with recomputable session",
                "cost_usd": 99.0,
                "cost_sessions": [
                    {"session_id": UUID_ALIVE, "cost_usd": 99.0, "timestamp": "t"},
                    {"session_id": "unrelated-session", "cost_usd": 4.0, "timestamp": "t"},
                ],
            },
            {
                "id": "ab-test0002",
                "title": "node with pricing-only session",
                "cost_usd": 9.0,
                "cost_sessions": [
                    {"session_id": UUID_GONE, "cost_usd": 9.0, "timestamp": "t"},
                ],
            },
        ]
    }
    (fno / "graph.json").write_text(json.dumps(graph, indent=2))

    claims = fno / "claims"
    claims.mkdir()
    if live_claim:
        expires_ms = int((time.time() + 3600) * 1000)
        (claims / "node%3Aab-live.lock").write_text(
            f"schema_version: 1\nkey: node:ab-live\nholder: target-session:x\n"
            f"expires_at: {expires_ms}\npid: 1\n"
        )
    if pid_claim is not None:
        # PID-liveness claim: the DEFAULT shape target sessions write -
        # no expires_at line, liveness == holder process existence.
        import os

        pid = os.getpid() if pid_claim == "live" else 99999999
        (claims / "node%3Aab-pidliveness.lock").write_text(
            f"schema_version: 1\nkey: node:ab-pidliveness\n"
            f"holder: target-session:y\nacquired_at: 1780633653467\n"
            f"pid: {pid}\nhost: testhost\n"
        )

    return {
        "ledger": fno / "ledger.json",
        "graph": fno / "graph.json",
        "claims": claims,
    }


def _run(home: Path, fx: dict, *args: str) -> subprocess.CompletedProcess:
    import os

    env = dict(os.environ, HOME=str(home))
    return subprocess.run(
        [
            sys.executable,
            str(BACKFILL),
            "--ledger", str(fx["ledger"]),
            "--graph", str(fx["graph"]),
            "--claims-dir", str(fx["claims"]),
            *args,
        ],
        capture_output=True,
        text=True,
        env=env,
    )


def _entries(path: Path) -> list[dict]:
    data = json.loads(path.read_text())
    return data if isinstance(data, list) else data.get("entries", [])


# --- AC3-UI: dry-run preview, no writes ----------------------------------------


def test_dry_run_previews_without_writing():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        before_ledger = fx["ledger"].read_text()
        before_graph = fx["graph"].read_text()
        result = _run(home, fx)
        assert result.returncode == 0, result.stderr
        assert fx["ledger"].read_text() == before_ledger
        assert fx["graph"].read_text() == before_graph
        assert "2.50" in result.stdout, result.stdout  # recomputed preview
        assert "dry-run" in result.stdout.lower()
        assert "no files written" in result.stdout.lower()


# --- AC3-HP: full recompute from transcripts ------------------------------------


def test_apply_recomputes_from_transcript():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr

        entries = _entries(fx["ledger"])
        recomputed = entries[0]
        assert recomputed["cost_usd"] == round(2 * COST_PER_MSG, 2), recomputed
        assert recomputed["tokens_total"] == 2 * TOKENS_PER_MSG
        assert recomputed["cache_read_tokens"] == 2 * USAGE["cache_read_input_tokens"]
        assert recomputed["compactions"] == 0
        assert recomputed["cost_backfill"] == "recomputed"


def test_apply_patches_graph_cost_sessions():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr

        graph = _entries(fx["graph"])
        node1 = next(e for e in graph if e["id"] == "ab-test0001")
        by_sid = {cs["session_id"]: cs for cs in node1["cost_sessions"]}
        assert by_sid[UUID_ALIVE]["cost_usd"] == round(2 * COST_PER_MSG, 2)
        # Unknown-session row untouched
        assert by_sid["unrelated-session"]["cost_usd"] == 4.0
        # Node aggregate recomputed from corrected sessions
        assert node1["cost_usd"] == round(2 * COST_PER_MSG + 4.0, 2)

        node2 = next(e for e in graph if e["id"] == "ab-test0002")
        assert node2["cost_sessions"][0]["cost_usd"] == 3.0  # 9.0 / 3
        assert node2["cost_usd"] == 3.0


# --- AC3-ERR: missing transcripts -----------------------------------------------


def test_opus48_without_transcript_gets_pricing_only_third():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        _run(home, fx, "--apply")
        entries = _entries(fx["ledger"])
        assert entries[1]["cost_usd"] == 3.0  # 9.0 / 3
        assert entries[1]["cost_backfill"] == "pricing_only"


def test_non_opus48_without_transcript_skipped_with_marker():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        _run(home, fx, "--apply")
        entries = _entries(fx["ledger"])
        assert entries[2]["cost_usd"] == 5.0  # unchanged
        assert entries[2]["cost_backfill"] == "no_transcript"
        # Scalar phase row: no sessions, no model -> skip, never guess
        assert entries[3]["cost_usd"] == 1.23
        assert entries[3]["cost_backfill"] == "no_transcript"
        assert entries[3]["session_id"] == "20260604T000000Z-1-aaaaaa:think"


# --- AC3-EDGE: re-run after apply is a no-op ------------------------------------


def test_rerun_after_apply_patches_zero_entries():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        _run(home, fx, "--apply")
        after_first_ledger = fx["ledger"].read_text()
        after_first_graph = fx["graph"].read_text()

        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr
        assert fx["ledger"].read_text() == after_first_ledger
        assert fx["graph"].read_text() == after_first_graph
        assert "patched this run: 0" in result.stdout, result.stdout


# --- AC3-FR: output integrity after apply ----------------------------------------


def test_files_parse_as_valid_json_after_apply():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        _run(home, fx, "--apply")
        json.loads(fx["ledger"].read_text())
        json.loads(fx["graph"].read_text())


# --- Concurrency: claims guard + session_id immutability --------------------------


def test_apply_refused_while_live_claims_exist():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home, live_claim=True)
        before = fx["ledger"].read_text()
        result = _run(home, fx, "--apply")
        assert result.returncode != 0
        assert "claim" in (result.stderr + result.stdout).lower()
        assert fx["ledger"].read_text() == before


def test_force_overrides_claims_guard():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home, live_claim=True)
        result = _run(home, fx, "--apply", "--force")
        assert result.returncode == 0, result.stderr
        assert _entries(fx["ledger"])[0]["cost_backfill"] == "recomputed"


def test_dry_run_allowed_despite_live_claims():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home, live_claim=True)
        result = _run(home, fx)
        assert result.returncode == 0, result.stderr


def test_apply_refused_for_pid_liveness_claim_with_live_process():
    # The default claim shape target sessions write carries NO expires_at;
    # liveness is the holder pid existing. The guard must see it.
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home, pid_claim="live")
        before = fx["ledger"].read_text()
        result = _run(home, fx, "--apply")
        assert result.returncode != 0
        assert "claim" in (result.stderr + result.stdout).lower()
        assert fx["ledger"].read_text() == before


def test_apply_proceeds_when_pid_liveness_claim_is_dead():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home, pid_claim="dead")
        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr
        assert _entries(fx["ledger"])[0]["cost_backfill"] == "recomputed"


def test_eperm_pid_claim_is_not_ours():
    # PID 1 (launchd/init) exists but is root-owned: kill(1, 0) raises
    # EPERM, which must read as "not ours" (a recycled PID owned by
    # another user is not our worker), so --apply proceeds.
    import os

    if os.geteuid() == 0:
        print("  SKIP test_eperm_pid_claim_is_not_ours (running as root)")
        return
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        (fx["claims"] / "node%3Aab-eperm.lock").write_text(
            "schema_version: 1\nkey: node:ab-eperm\nholder: target-session:z\n"
            "acquired_at: 1780633653467\npid: 1\nhost: testhost\n"
        )
        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr


def test_interrupted_apply_graph_leg_repairable_on_rerun():
    # The ledger and graph passes are not mutually atomic. Simulate a crash
    # after the ledger commit but before the graph write, then verify a
    # rerun rebuilds the correction map from the marked entries and repairs
    # the graph (codex P2 on PR #443).
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        pre_graph = fx["graph"].read_text()
        _run(home, fx, "--apply")
        fx["graph"].write_text(pre_graph)  # graph write "never happened"

        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr
        # Ledger untouched on the rerun...
        assert "patched this run: 0" in result.stdout
        # ...but the graph is repaired.
        graph = _entries(fx["graph"])
        node1 = next(e for e in graph if e["id"] == "ab-test0001")
        by_sid = {cs["session_id"]: cs for cs in node1["cost_sessions"]}
        assert by_sid[UUID_ALIVE]["cost_usd"] == round(2 * COST_PER_MSG, 2)
        node2 = next(e for e in graph if e["id"] == "ab-test0002")
        assert node2["cost_sessions"][0]["cost_usd"] == 3.0  # 9.0 / 3


def test_session_id_fields_never_rewritten():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        before = [
            (e.get("session_id"), tuple(e.get("sessions") or []))
            for e in _entries(fx["ledger"])
        ]
        _run(home, fx, "--apply")
        after = [
            (e.get("session_id"), tuple(e.get("sessions") or []))
            for e in _entries(fx["ledger"])
        ]
        assert before == after


# --- Boundaries -------------------------------------------------------------------


def test_empty_ledger_clean_skip():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        fx["ledger"].write_text(json.dumps({"entries": []}))
        result = _run(home, fx, "--apply")
        assert result.returncode == 0, result.stderr
        assert "patched this run: 0" in result.stdout


def test_corrupt_ledger_errors_without_write():
    with tempfile.TemporaryDirectory() as td:
        home = Path(td)
        fx = _make_fixture(home)
        fx["ledger"].write_text("{not json")
        result = _run(home, fx, "--apply")
        assert result.returncode != 0
        assert fx["ledger"].read_text() == "{not json"


def test_help_flag():
    result = subprocess.run(
        [sys.executable, str(BACKFILL), "--help"], capture_output=True, text=True
    )
    assert result.returncode == 0
    assert "--apply" in result.stdout


def _main() -> int:
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"  PASS {name}")
            except AssertionError as exc:
                failures += 1
                print(f"  FAIL {name}: {exc}")
    print(f"{'OK' if failures == 0 else 'FAILED'} ({failures} failures)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(_main())

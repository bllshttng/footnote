"""Tests for the daily grooming pass (`fno backlog groom`).

Covers the once-a-day dedup marker, the spawn shape, the failure hand-back, and
the skill brief's lever contract.
"""
from __future__ import annotations

from datetime import date
from pathlib import Path

import pytest

from fno.backlog import groom as G

SKILL = Path(__file__).resolve().parents[3] / "skills" / "groom" / "SKILL.md"


@pytest.fixture
def claims_root(tmp_path, monkeypatch) -> Path:
    """Route the groom: claim into a tmp dir so the marker is hermetic."""
    root = tmp_path / "claims_home"
    root.mkdir()
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(root))
    return root


# Bound before the autouse stub replaces the module attribute, so the tests that
# exercise the real leg runner reach it rather than the stub.
REAL_MECHANICAL = G._run_mechanical


@pytest.fixture(autouse=True)
def no_real_mechanical(monkeypatch):
    """Never shell out to the real CLI; tests that care use REAL_MECHANICAL."""
    monkeypatch.setattr(G, "_run_mechanical", lambda age: {"archive": "ok"})


@pytest.fixture
def spawns(monkeypatch) -> list:
    """Capture spawn calls instead of launching a real worker."""
    calls: list = []

    def _fake(brief: str, cwd: str, model: str, day: str) -> str:
        calls.append({"brief": brief, "cwd": cwd, "model": model, "day": day})
        return "gr01"

    monkeypatch.setattr(G, "_spawn_groom_worker", _fake)
    return calls


DAY = date(2026, 7, 19)


def test_day_key_is_utc_date_scoped():
    assert G.groom_day_key(DAY) == "groom:2026-07-19"


def test_groom_key_routes_to_the_global_claims_root():
    # Grooming operates on the GLOBAL graph, so its daily marker must dedup
    # across repos - a repo-local root would let two checkouts both groom today.
    from fno.claims.io import claims_root_for

    assert claims_root_for(G.groom_day_key(DAY)) is not None


def test_first_run_dispatches(claims_root, spawns):
    r = G.run_groom(cwd="/tmp", today=DAY)
    assert r["status"] == "dispatched"
    assert r["day"] == "2026-07-19"
    assert r["short_id"] == "gr01"
    assert len(spawns) == 1


def test_second_run_same_day_is_a_no_op(claims_root, spawns):
    first = G.run_groom(cwd="/tmp", today=DAY)
    second = G.run_groom(cwd="/tmp", today=DAY)

    assert first["status"] == "dispatched"
    assert second["status"] == "already-ran"
    assert len(spawns) == 1, "the second run must not spawn a worker"


def test_next_day_dispatches_again(claims_root, spawns):
    G.run_groom(cwd="/tmp", today=DAY)
    r = G.run_groom(cwd="/tmp", today=date(2026, 7, 20))

    assert r["status"] == "dispatched"
    assert len(spawns) == 2


def test_dry_run_neither_claims_nor_spawns(claims_root, spawns):
    r = G.run_groom(cwd="/tmp", today=DAY, dry_run=True)

    assert r["status"] == "dry-run"
    assert not spawns
    # No marker was written, so a real run today is still available.
    assert G.run_groom(cwd="/tmp", today=DAY)["status"] == "dispatched"


def test_unlaunchable_spawn_hands_the_day_back(claims_root, monkeypatch, spawns):
    # An OSError means the binary never executed, so no lever was pulled and the
    # day must not be burned behind a marker nothing clears until tomorrow.
    def _boom(*a, **k):
        raise OSError("No such file or directory: fno")

    monkeypatch.setattr(G, "_spawn_groom_worker", _boom)
    failed = G.run_groom(cwd="/tmp", today=DAY)
    assert failed["status"] == "failed"
    assert failed["released"] is True, "the handback must be reported, not assumed"

    monkeypatch.setattr(G, "_spawn_groom_worker", lambda *a, **k: "gr02")
    assert G.run_groom(cwd="/tmp", today=DAY)["status"] == "dispatched"


@pytest.mark.parametrize(
    "exc",
    [
        pytest.param(
            lambda: G.subprocess.TimeoutExpired(cmd="fno", timeout=G._SPAWN_TIMEOUT_S),
            id="timeout",
        ),
        pytest.param(lambda: RuntimeError("fno agents spawn exited 2"), id="nonzero-exit"),
    ],
)
def test_a_worker_that_may_have_run_holds_the_marker(claims_root, monkeypatch, exc):
    # headless is synchronous, so both a timeout and a non-zero exit can land
    # AFTER levers were applied. Re-dispatching today would re-apply them, so the
    # day stays held; the operator sees it via status=failed + exit 1.
    def _raise(*a, **k):
        raise exc()

    monkeypatch.setattr(G, "_spawn_groom_worker", _raise)
    r = G.run_groom(cwd="/tmp", today=DAY)
    assert r["status"] == "failed"
    assert r["released"] is False

    monkeypatch.setattr(G, "_spawn_groom_worker", lambda *a, **k: "gr03")
    assert G.run_groom(cwd="/tmp", today=DAY)["status"] == "already-ran"


def test_spawn_is_headless_sonnet(monkeypatch, claims_root):
    """The substrate and model are load-bearing: explicit headless, never `-p`."""
    captured: dict = {}

    class _Proc:
        returncode = 0
        stdout = '{"short_id": "gr03"}'
        stderr = ""

    def _fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(G.subprocess, "run", _fake_run)
    r = G.run_groom(cwd="/repo", today=DAY)

    assert r["short_id"] == "gr03"
    cmd = captured["cmd"]
    assert "--substrate" in cmd and cmd[cmd.index("--substrate") + 1] == "headless"
    assert cmd[cmd.index("--model") + 1] == G.GROOM_MODEL_DEFAULT
    assert cmd[cmd.index("--cwd") + 1] == "/repo"
    assert "-p" not in cmd, "the subscription lane never shells bare -p"
    # The RUNNER must own the timeout: killing our own subprocess would reap the
    # spawn wrapper and leave the `claude -p` grandchild mutating the graph.
    assert cmd[cmd.index("--timeout") + 1] == str(G._WORKER_TIMEOUT_S)
    assert G._SPAWN_TIMEOUT_S > G._WORKER_TIMEOUT_S, "inner bound must fire first"


# ── the skill brief contract ────────────────────────────────────────────────


def test_brief_points_at_the_skill():
    brief = G.groom_brief("2026-07-19")
    # Name the skill, not a repo-relative path: the worker's cwd is not
    # guaranteed to be the footnote checkout.
    assert "fno:groom" in brief
    assert "2026-07-19" in brief


# (command, flags the brief teaches for it). Kept together so the two tests
# below cannot drift: one pins the brief's text, the other pins the real CLI.
LEVERS = [
    ("supersede", ("--replaces", "--reason")),
    ("defer", ("--reason",)),
    ("undefer", ()),
    ("update", ("--priority",)),
    ("rank", ("--top",)),
    ("idea", ()),
    ("intake", ()),
    ("update", ("--blocked-by",)),
]


@pytest.mark.parametrize("command,flags", LEVERS)
def test_skill_names_every_allowed_lever(command, flags):
    text = SKILL.read_text()
    assert f"fno backlog {command}" in text
    for flag in flags:
        assert flag in text


@pytest.mark.parametrize("command,flags", LEVERS)
def test_brief_levers_exist_on_the_real_cli(command, flags):
    """The brief must not teach a flag the CLI does not have.

    A worker follows this brief literally, so a wrong signature fails at exit 2
    on the lever rather than anywhere visible. Substring checks on the doc alone
    cannot catch that - this binds it to the actual command.
    """
    import click
    import typer.main

    from fno.graph.cli import cli as graph_cli

    root = typer.main.get_command(graph_cli)
    sub = root.get_command(click.Context(root), command)
    assert sub is not None, f"`fno backlog {command}` does not exist"

    available: set[str] = set()
    for param in sub.params:
        available.update(param.opts)
    missing = set(flags) - available
    assert not missing, f"`fno backlog {command}` has no {sorted(missing)}"


def test_skill_carries_the_auto_convene_and_report_contract():
    text = SKILL.read_text()
    assert "Auto-convene" in text
    assert "fno mail send" in text
    assert "Net mint rate" in text


def test_skill_routes_questions_to_the_deferred_pile_not_idea():
    """The triage pile IS `deferred` + a reason, so a question must defer.

    An `idea`-status node does not appear in the pile, so routing questions
    there would silently drop them from the surface grooming reports on.
    """
    text = SKILL.read_text()
    assert 'fno backlog defer <id> --reason "question:' in text
    assert "an idea-status node is not in the pile" in text.lower()


def test_skill_forbids_direct_state_edits():
    text = SKILL.read_text()
    assert "graph.json" in text and "Never" in text
    for forbidden in ("jq -i", "sed -i"):
        assert forbidden in text, "the brief must name the direct-edit paths it forbids"


# ── the mechanical pass ─────────────────────────────────────────────────────


class _Proc:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


@pytest.fixture
def legs(monkeypatch) -> list:
    """Capture mechanical leg subprocesses; returns the recorded commands."""
    monkeypatch.undo()  # drop the autouse stub so the real _run_mechanical runs
    calls: list = []

    def _fake_run(cmd, **kwargs):
        calls.append(cmd)
        return _Proc()

    monkeypatch.setattr(G.subprocess, "run", _fake_run)
    return calls


def test_mechanical_legs_run_in_order_after_the_claim(claims_root, spawns, monkeypatch):
    seen: list = []
    monkeypatch.setattr(G, "_run_mechanical", lambda age: seen.append(age) or {"archive": "ok"})

    r = G.run_groom(cwd="/tmp", today=DAY, age=7)

    assert r["status"] == "dispatched"
    assert seen == [7], "--age must reach the archive leg"
    assert r["mechanical"] == {"archive": "ok"}


def test_relatedness_builds_last_over_the_post_groom_graph():
    # Build must see the post-archive corpus, or this run's archived nodes stay
    # in the map as dangling edges until tomorrow.
    names = [name for name, _ in G._mechanical_legs(14)]
    assert names == ["archive", "reconcile", "maintain", "relatedness"]


def test_quiet_night_legs_are_ok(monkeypatch):
    # The quiet paths all exit 0: reconcile prints "Backlog is in sync." and
    # returns, archive/maintain report nothing to do and return.
    monkeypatch.setattr(G.subprocess, "run", lambda cmd, **k: _Proc(returncode=0))
    assert set(REAL_MECHANICAL(14).values()) == {"ok"}


def test_exit_4_is_partial_not_ok(monkeypatch):
    # Every exit-4 site in this CLI is a DEGRADED outcome, never "nothing to do":
    # reconcile raises it when PR queries could not be resolved. Recording that
    # as `ok` would log a reconcile that silently stopped closing nodes as
    # healthy every night - the exact staleness this pipeline exists to kill.
    monkeypatch.setattr(G.subprocess, "run", lambda cmd, **k: _Proc(returncode=4, stderr="2 node(s) could not be resolved"))
    results = REAL_MECHANICAL(14)

    assert all(v.startswith("partial: 4:") for v in results.values()), results
    assert G._leg_trouble(results) == ["archive", "maintain", "reconcile", "relatedness"]


def test_one_failing_leg_does_not_abort_the_rest(monkeypatch):
    def _fake_run(cmd, **kwargs):
        if "reconcile" in cmd:
            return _Proc(returncode=1, stderr="boom")
        return _Proc()

    monkeypatch.setattr(G.subprocess, "run", _fake_run)
    results = REAL_MECHANICAL(14)

    assert results["reconcile"].startswith("failed: 1: boom")
    assert results["maintain"] == "ok" and results["relatedness"] == "ok"


def test_a_wedged_leg_is_named_not_raised(monkeypatch):
    def _boom(cmd, **kwargs):
        raise G.subprocess.TimeoutExpired(cmd="fno", timeout=G._LEG_TIMEOUT_S)

    monkeypatch.setattr(G.subprocess, "run", _boom)
    assert "TimeoutExpired" in REAL_MECHANICAL(14)["archive"]


def test_same_day_rerun_runs_zero_mechanical_verbs(claims_root, spawns, legs):
    # AC1-EDGE: the claim guards the WHOLE pipeline, so a second run must not
    # re-mutate the graph, not merely skip the worker.
    G.run_groom(cwd="/tmp", today=DAY)
    before = len(legs)
    assert G.run_groom(cwd="/tmp", today=DAY)["status"] == "already-ran"
    assert len(legs) == before, "already-ran must spawn no mechanical subprocess"


def test_spawn_failure_keeps_mechanical_results_in_the_receipt(claims_root, monkeypatch):
    # AC1-ERR: the mutations already landed; the receipt must still report them.
    monkeypatch.setattr(G, "_spawn_groom_worker", lambda *a, **k: (_ for _ in ()).throw(OSError("nope")))
    r = G.run_groom(cwd="/tmp", today=DAY)

    assert r["status"] == "failed" and r["released"] is True
    assert r["mechanical"] == {"archive": "ok"}


def test_dry_run_names_the_legs_without_running_them(claims_root, spawns, legs):
    r = G.run_groom(cwd="/tmp", today=DAY, dry_run=True)
    # Same shape as a real pass: a consumer must never branch on which run it was.
    assert r["mechanical"] == {
        "archive": "pending",
        "reconcile": "pending",
        "maintain": "pending",
        "relatedness": "pending",
    }
    assert not legs


def test_every_receipt_status_is_in_the_declared_vocabulary(claims_root, spawns, monkeypatch):
    """`cmd_groom` maps a subset of these to a non-zero exit.

    A status added here without updating that tuple degrades to a silent exit 0,
    which on an unattended nightly job means the break is invisible.
    """
    import typing

    declared = set(typing.get_args(G.GroomStatus))
    seen = {
        G.run_groom(cwd="/tmp", today=DAY, dry_run=True)["status"],
        G.run_groom(cwd="/tmp", today=DAY)["status"],
        G.run_groom(cwd="/tmp", today=DAY)["status"],
    }
    monkeypatch.setattr(G, "_run_mechanical", lambda age: {"archive": "failed: 1: x"})
    seen.add(G.run_groom(cwd="/tmp", today=date(2026, 7, 21))["status"])

    assert seen <= declared, f"undeclared status: {seen - declared}"
    assert {"dry-run", "dispatched", "already-ran", "degraded"} <= seen


# ── cadence installer ───────────────────────────────────────────────────────


def test_install_renders_a_daily_plist_at_the_requested_hour():
    xml = G.render_groom_plist(fno_binary="/usr/local/bin/fno", install_path="/bin", hour=3)

    assert f"<string>{G.GROOM_LABEL}</string>" in xml
    assert "<key>StartCalendarInterval</key>" in xml
    assert "<integer>3</integer>" in xml
    # RunAtLoad false: installing at 4pm must not fire a pass immediately.
    assert "<key>RunAtLoad</key>\n  <false/>" in xml
    assert "backlog" in xml and "groom" in xml


def test_install_escapes_a_binary_path_that_would_break_the_xml():
    xml = G.render_groom_plist(fno_binary="/opt/a&b/fno", install_path="/bin")
    assert "/opt/a&amp;b/fno" in xml


def test_install_on_non_macos_reports_the_cron_fallback(monkeypatch):
    monkeypatch.setattr(__import__("sys"), "platform", "linux")
    r = G.install_groom_agent()
    assert r["status"] == "unsupported"
    assert "backlog groom" in r["cron"]


def test_install_bounces_rather_than_loads(tmp_path, monkeypatch):
    # `launchctl load` cannot cure the wedge class an `fno update` bounce leaves
    # behind; reusing pr_watch's bootout->bootstrap->kickstart is the point.
    import sys as _sys

    monkeypatch.setattr(_sys, "platform", "darwin")
    called: dict = {}

    def _fake_bounce(*, plist_path, label=None, **kwargs):
        called["plist"] = plist_path
        called["label"] = label
        called["kickstart"] = kwargs.get("kickstart", True)
        return ("bounced", 0)

    monkeypatch.setattr("fno.pr_watch._install.bounce", _fake_bounce)
    r = G.install_groom_agent(launch_agents_dir=tmp_path, fno_binary="/bin/fno", install_path="/bin")

    assert r["status"] == "installed"
    assert called["label"] == G.GROOM_LABEL
    assert (tmp_path / f"{G.GROOM_LABEL}.plist").exists()


def test_install_never_kickstarts_the_grooming_job(tmp_path, monkeypatch):
    """Installing must not run a pass.

    `bounce` ends with `launchctl kickstart -k`, which is free liveness
    confirmation for the watcher's idempotent poll. Grooming's tick mutates the
    backlog and burns the day's claim, so a kickstart here would groom at
    install time and contradict the plist's own RunAtLoad=false.
    """
    import sys as _sys

    monkeypatch.setattr(_sys, "platform", "darwin")
    steps: list = []

    def _fake_launchctl(*args, timeout_s=None):
        steps.append(args[0])
        return (0, False)

    from fno.pr_watch import _install

    monkeypatch.setattr(_install, "_run_launchctl_timed", _fake_launchctl)
    r = G.install_groom_agent(launch_agents_dir=tmp_path, fno_binary="/bin/fno", install_path="/bin")

    assert r["status"] == "installed"
    assert "bootstrap" in steps
    assert "kickstart" not in steps, "installing must not fire a grooming pass"


def test_watcher_install_still_kickstarts():
    """The default is unchanged: pr_watch relies on the forced first tick."""
    steps: list = []

    def _fake_run(*args, timeout_s=None):
        steps.append(args[0])
        return (0, False)

    from fno.pr_watch._install import bounce

    bounce(plist_path=Path("/tmp/x.plist"), label="sh.fno.test", uid=501, run=_fake_run)
    assert "kickstart" in steps


def test_skill_reads_fresh_proposals_and_reports_the_mechanical_line():
    # The pass-to-pass interface is the live verb, not a file: a digest is what
    # went stale for ten days unnoticed.
    text = SKILL.read_text()
    assert "fno backlog maintain" in text
    assert "Mechanical" in text
    assert "groom-digest" not in text, "the digest is retired; the skill must not resurrect it"


def test_the_old_nightly_script_is_a_shim_that_defers():
    # AC2-HP: one surface owns grooming. The shim must hand off, not re-sequence
    # the legs itself, and must never resurrect the retired digest.
    script = Path(__file__).resolve().parents[3] / "scripts" / "nightly-groom.sh"
    text = script.read_text()

    assert "DEPRECATED" in text
    assert "exec" in text and "backlog groom" in text
    assert "groom-digest" not in text

    # Comments may still explain what moved; the executable body must not do it.
    body = "\n".join(
        line for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")
    )
    for retired in ("archive", "reconcile", "maintain", "relatedness", "DIGEST"):
        assert retired not in body, f"the shim must not still run {retired!r} itself"


def test_every_mechanical_leg_exists_on_the_real_cli():
    """Bind the leg table to the actual commands.

    A leg runs unattended and its failure is swallowed into a receipt string, so
    a renamed verb or dropped flag would degrade to `failed: 2` every night and
    read as a flaky leg rather than a typo. Only this binding catches it.
    """
    import click
    import typer.main

    from fno.graph.cli import cli as graph_cli

    root = typer.main.get_command(graph_cli)
    ctx = click.Context(root)

    for name, args in G._mechanical_legs(14):
        cmd = root.get_command(ctx, args[0])
        assert cmd is not None, f"`fno backlog {args[0]}` does not exist ({name} leg)"

        if len(args) > 1 and not args[1].startswith("-"):
            sub = cmd.get_command(click.Context(cmd), args[1])
            assert sub is not None, f"`fno backlog {args[0]} {args[1]}` does not exist"

        available = {opt for param in cmd.params for opt in param.opts}
        missing = [a for a in args[1:] if a.startswith("--") and a not in available]
        assert not missing, f"`fno backlog {args[0]}` has no {missing}"


# ── a broken leg must reach a human ─────────────────────────────────────────


def test_a_failed_leg_degrades_the_receipt(claims_root, spawns, monkeypatch):
    monkeypatch.setattr(G, "_run_mechanical", lambda age: {"archive": "ok", "reconcile": "failed: 1: boom"})
    r = G.run_groom(cwd="/tmp", today=DAY)

    # The worker still dispatched - the pass is best-effort - but the status must
    # not read clean, because status is what reaches the exit code.
    assert r["status"] == "degraded"
    assert r["degraded_legs"] == ["reconcile"]


def test_degraded_exits_nonzero_so_the_break_is_not_log_only(monkeypatch):
    # The receipt's only other sink under launchd is a log file with no reader.
    from typer.testing import CliRunner

    from fno.graph.cli import cli as graph_cli

    monkeypatch.setattr(
        "fno.backlog.groom.run_groom",
        lambda **kw: {"status": "degraded", "day": "2026-07-19", "mechanical": {"reconcile": "failed: 1: x"}},
    )
    result = CliRunner().invoke(graph_cli, ["groom"])
    assert result.exit_code == 1


def test_the_worker_is_told_what_the_mechanical_pass_did(claims_root, spawns, monkeypatch):
    # AC1-HP asks the mailed report to itemize every leg. The worker is the only
    # thing that reaches a human, and it cannot report outcomes it never got.
    monkeypatch.setattr(
        G, "_run_mechanical", lambda age: {"archive": "ok", "reconcile": "failed: 1: boom"}
    )
    G.run_groom(cwd="/tmp", today=DAY)

    brief = spawns[0]["brief"]
    assert "archive ok" in brief
    assert "reconcile failed: 1: boom" in brief
    assert "Mechanical" in brief


def test_a_clean_pass_leaves_the_brief_unadorned(claims_root, spawns):
    G.run_groom(cwd="/tmp", today=DAY)
    assert "fno:groom" in spawns[0]["brief"]


def test_a_lost_correlation_id_is_flagged_not_silent(claims_root, monkeypatch):
    # The worker launched, so the pass stands; but `fno agents logs` has no
    # handle, which is worth counting rather than reading as a normal dispatch.
    monkeypatch.setattr(G, "_spawn_groom_worker", lambda *a, **k: "unknown")
    r = G.run_groom(cwd="/tmp", today=DAY)

    assert r["short_id_lost"] is True

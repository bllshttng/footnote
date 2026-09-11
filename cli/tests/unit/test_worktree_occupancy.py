"""Rules R0-R5 over a fabricated process table; no live processes (x-0396)."""

import pytest

from fno.worktree_occupancy import INERT, RETIRE, TERMINATE, classify


HOME = "/Users/tester"


def row(pid, ppid, argv, **extra):
    d = {"pid": pid, "ppid": ppid, "name": argv[0].rsplit("/", 1)[-1] if argv else "", "cmdline": argv}
    d.update(extra)
    return d


def run(pids, procs, keeper_verdicts=None, job_of_pid=None, job_state=None, home=HOME):
    return classify(
        pids,
        procs=procs,
        keeper_verdicts=keeper_verdicts,
        job_of_pid=job_of_pid,
        job_state=job_state,
        home=home,
        now=1_000_000.0,
    )


def by_pid(hits):
    return {h.pid: h for h in hits}


KEEPER = row(10, 1, ["/usr/local/bin/fno-agents-worker", "--pane", "--sock", "/tmp/fno-mux-test-x/panes/test-1.sock", "--", "/bin/cat"])
CAT = row(11, 10, ["/bin/cat"])
REAP_VERDICT = ("reap", "socket absent, no registry row claims it, age 90000s > grace 600s")


class TestKeeper:
    def test_reap_keeper_and_cat_child_release_the_tree(self):
        """AC1-HP: REAP keeper plus its /bin/cat child are both inert terminate."""
        hits = by_pid(run([10, 11], {10: KEEPER, 11: CAT}, keeper_verdicts={10: REAP_VERDICT}))
        assert hits[10].verdict == INERT and hits[10].action == TERMINATE
        assert hits[11].verdict == INERT and hits[11].action == TERMINATE
        assert hits[11].reason.startswith("child of 10: ")
        assert hits[10].reason == REAP_VERDICT[1]

    def test_broken_lane_keeps_the_keeper(self):
        """AC7-EDGE: a keeper with no verdict (lane broken) holds."""
        hits = by_pid(run([10], {10: KEEPER}, keeper_verdicts={}))
        assert (hits[10].verdict, hits[10].action) == ("holds", "keep")

    def test_leave_verdict_keeps_the_keeper(self):
        hits = by_pid(run([10], {10: KEEPER}, keeper_verdicts={10: ("leave", "registry row ci-1 claims this keeper")}))
        assert (hits[10].verdict, hits[10].action) == ("holds", "keep")
        assert "ci-1" in hits[10].reason


SPARE_ARGV = ["claude", "bg-spare", "--bg-spare", "/tmp/cc-daemon-501/d/spare/s.claim.sock"]


def job_state_factory(states):
    def read(job_id):
        return states[job_id]

    return read


class TestClaudeSession:
    def test_working_blocked_and_fresh_done_hold_with_job_id_and_state(self):
        """AC3-HP: the positive control - a live-argv bg session holds."""
        for state, age in (("working", 5.0), ("blocked", 60.0), ("done", 600.0)):
            procs = {30: row(30, 1, SPARE_ARGV, cwd="/Users/tester/wt/x-live")}
            hits = by_pid(
                run([30], procs, job_of_pid={30: "abc123"}, job_state=job_state_factory({"abc123": (state, age)}))
            )
            assert (hits[30].verdict, hits[30].action) == ("holds", "keep"), state
            assert "abc123" in hits[30].reason and state in hits[30].reason

    def test_terminal_silent_job_releases_the_tree(self):
        """AC4-HP: terminal state + transcript silent past 7200s -> inert retire."""
        for state in ("done", "stopped", "failed"):
            hits = by_pid(
                run([30], {30: row(30, 1, SPARE_ARGV)}, job_of_pid={30: "abc123"},
                    job_state=job_state_factory({"abc123": (state, 7201.0)}))
            )
            assert hits[30].verdict == INERT and hits[30].action == RETIRE, state
            assert hits[30].job_id == "abc123"

    def test_join_failures_hold_even_when_a_cwd_would_say_free(self):
        """AC5-EDGE: unreadable map, absent pid, unreadable state.json all hold.

        The fabricated rows carry cwd values pointing INSIDE a worktree - the
        registry cwd and state.json cwd are spawn directories and the
        classifier never reads them, so they cannot flip a row to free."""
        procs = {30: row(30, 1, SPARE_ARGV, cwd="/Users/tester/wt/x-live")}
        unreadable = job_state_factory({})

        cases = [
            run([30], procs, job_of_pid=None, job_state=unreadable),
            run([30], procs, job_of_pid={}, job_state=unreadable),
            run([30], procs, job_of_pid={99: "abc123"}, job_state=unreadable),
            run([30], procs, job_of_pid={30: "nojob"}, job_state=job_state_factory({"nojob": None})),
        ]
        for hits in cases:
            h = by_pid(hits)[30]
            assert (h.verdict, h.action) == ("holds", "keep")

    def test_child_of_a_working_session_holds(self):
        """R4: caffeinate under a live bg session inherits holds."""
        procs = {
            30: row(30, 1, SPARE_ARGV),
            31: row(31, 30, ["caffeinate", "-i", "-t", "300"]),
        }
        hits = by_pid(
            run([31], procs, job_of_pid={30: "abc123"}, job_state=job_state_factory({"abc123": ("working", 1.0)}))
        )
        assert (hits[31].verdict, hits[31].action) == ("holds", "keep")
        assert hits[31].reason.startswith("child of 30: claude job abc123 working")

    def test_child_of_a_retiring_session_retires_with_the_job(self):
        procs = {
            30: row(30, 1, SPARE_ARGV),
            31: row(31, 30, ["/bin/cat"]),
        }
        hits = by_pid(
            run([31], procs, job_of_pid={30: "abc123"}, job_state=job_state_factory({"abc123": ("done", 99999.0)}))
        )
        assert hits[31].verdict == INERT and hits[31].action == RETIRE and hits[31].job_id == "abc123"


class TestOrphanedShell:
    def test_snapshot_shell_and_sleep_child_release_the_tree(self):
        """AC2-HP: ppid-1 zsh sourcing a shell snapshot plus its sleep child."""
        zsh = row(20, 1, ["/bin/zsh", "-c", f"source {HOME}/.claude/shell-snapshots/snapshot-zsh.sh; sleep 5"])
        sleep = row(21, 20, ["sleep", "5"])
        hits = by_pid(run([20, 21], {20: zsh, 21: sleep}))
        assert (hits[20].verdict, hits[20].action) == (INERT, TERMINATE)
        assert (hits[21].verdict, hits[21].action) == (INERT, TERMINATE)
        assert hits[21].reason.startswith("child of 20: ")

    def test_tilde_spelling_also_matches(self):
        zsh = row(20, 1, ["/bin/zsh", "-c", "source ~/.claude/shell-snapshots/snapshot-zsh.sh; sleep 5"])
        hits = by_pid(run([20], {20: zsh}))
        assert (hits[20].verdict, hits[20].action) == (INERT, TERMINATE)

    def test_live_shell_with_a_parent_is_not_orphaned(self):
        zsh = row(20, 78949, ["/bin/zsh", "-c", f"source {HOME}/.claude/shell-snapshots/snapshot-zsh.sh; true"])
        hits = by_pid(run([20], {20: zsh}))
        assert (hits[20].verdict, hits[20].action) == ("holds", "keep")


class TestUnclassified:
    def test_unmatched_programs_hold_with_their_name(self):
        """AC6-EDGE: vim, a bare cargo, an unplaceable zsh."""
        procs = {
            40: row(40, 1, ["vim", "notes.txt"]),
            41: row(41, 1, ["cargo", "build"]),
            42: row(42, 42, ["/bin/zsh", "-c", f"source {HOME}/.claude/shell-snapshots/snapshot-zsh.sh; true"]),
        }
        hits = by_pid(run([40, 41, 42], procs))
        assert hits[40].reason == "unclassified: vim"
        assert hits[41].reason == "unclassified: cargo"
        assert hits[42].reason == "unclassified: zsh"
        for h in hits.values():
            assert (h.verdict, h.action) == ("holds", "keep")

    def test_pid_absent_from_the_table_holds(self):
        """AC6-EDGE: no ps row is a holder, never a free answer."""
        hits = run([50], {})
        h = hits[0]
        assert (h.verdict, h.action) == ("holds", "keep") and h.reason == "no ps row"


def test_holds_and_inert_vocabularies():
    """The sweep bridge matches on these exact words; pin them."""
    from fno import worktree_occupancy as m

    assert {m.HOLDS, m.INERT} == {"holds", "inert"}
    assert {m.KEEP, m.TERMINATE, m.RETIRE} == {"keep", "terminate", "retire"}


def test_no_registry_or_cwd_read_in_module():
    """Verify step 6: the only cwd/registry mentions are the trap comment."""
    import pathlib

    src = pathlib.Path(__file__).parents[2] / "src" / "fno" / "worktree_occupancy.py"
    bad = []
    for i, line in enumerate(src.read_text().splitlines(), 1):
        if '"cwd"' in line or ".cwd" in line or "registry" in line.lower():
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            bad.append((i, stripped))
    assert bad == [], bad


@pytest.mark.parametrize("pid", [1, 0, -1])
def test_ppid_walk_ignores_nonpositive_and_self(pid):
    procs = {30: row(30, pid, SPARE_ARGV)}
    hits = by_pid(run([30], procs))
    assert (hits[30].verdict, hits[30].action) == ("holds", "keep")

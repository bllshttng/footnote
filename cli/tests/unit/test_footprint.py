"""Fixture-driven tests for the machine-footprint parser."""

from __future__ import annotations

from fno.footprint import parse_footprint


def test_ac3_edge_truncated_new_format_row_is_unparsed() -> None:
    reading = parse_footprint(
        """\
        PID PPID ELAPSED %CPU RSS COMMAND
        101 1 5 20 fno --version
        """,
    )

    assert reading.unparsed_lines == 1
    assert reading.process_count == 0


def test_ac1_hp_attributes_transitive_build_descendants_and_excludes_negative_tree() -> None:
    reading = parse_footprint(
        """\
        PID PPID ELAPSED %CPU RSS COMMAND
        100 1 01:00:00 20.0 1024 fno-agents-worker --run
        101 100 01:00:00 0.0 1024 /bin/sh -c cargo test
        102 101 00:00:05 80.0 1024 cargo test -p fno
        103 101 00:00:05 60.0 1024 rustc --crate-name fno
        200 1 01:00:00 0.0 1024 /bin/sh -c cargo test elsewhere
        201 200 01:00:00 90.0 1024 cargo test -p unrelated
        202 200 01:00:00 70.0 1024 rustc --crate-name unrelated
        """,
    )

    assert reading.descendant_cpu_cores == 1.4
    assert reading.fleet_cpu_cores == 1.6
    assert reading.descendant_process_count == 3
    assert reading.direct_process_count == 1
    assert reading.process_count == 4
    assert [command for _, command in reading.top] == [
        "cargo test -p fno",
        "rustc --crate-name fno",
        "fno-agents-worker --run",
    ]


def test_ac1_edge_attributes_detached_registered_root_and_descendants() -> None:
    ps_output = """\
        PID PPID ELAPSED %CPU RSS COMMAND
        300 1 00:00:05 20.0 1024 opencode serve --detach
        301 300 00:01:00 80.0 1024 cargo test -p fno
        400 1 01:00:00 90.0 1024 cargo test -p unrelated
    """

    reading = parse_footprint(ps_output, attributed_root_pids={300})

    assert reading.process_count == 2
    assert reading.direct_process_count == 1
    assert reading.descendant_process_count == 1
    assert reading.sustained_cpu_cores == 0.2
    assert reading.transient_call_count == 0
    assert reading.descendant_cpu_cores == 0.8
    assert reading.fleet_cpu_cores == 1.0
    assert [command for _, command in reading.top] == [
        "cargo test -p fno",
        "opencode serve --detach",
    ]


def test_ac1_edge_shared_serve_is_not_a_direct_threshold_process() -> None:
    reading = parse_footprint(
        """\
        PID PPID ELAPSED %CPU RSS COMMAND
        300 1 01:00:00 20.0 1024 fno-agents-worker --run
        400 1 01:00:00 0.0 1024 opencode serve --detach
        401 400 01:00:00 10.0 1024 cargo test -p fno
        """,
        attributed_root_pids={300, 400},
        threshold_excluded_root_pids={400},
    )

    assert reading.process_count == 3
    assert reading.direct_process_count == 1
    assert reading.descendant_process_count == 1
    assert reading.fleet_cpu_cores == 0.3


def test_ac3_edge_inspects_pid_one_as_an_attribution_root() -> None:
    reading = parse_footprint(
        """\
        PID PPID ELAPSED %CPU RSS COMMAND
        1 0 01:00:00 20.0 1024 fno-agents-worker --run
        2 1 01:00:00 80.0 1024 cargo test -p fno
        """
    )

    assert reading.process_count == 2
    assert reading.fleet_cpu_cores == 1.0
    assert reading.descendant_process_count == 1


def test_ac1_hp_counts_attributed_rows_and_sustained_cpu() -> None:
    reading = parse_footprint(
        """\
        PID ELAPSED %CPU RSS COMMAND
        101 01:00:00 20.0 1048576 /usr/local/bin/fno mux serve
        102 00:00:05 90.0 2048 fno --version
        103 02:00:00 80.0 4096 claude --worker
        104 00:45:00 10.0 524288 fno-agents-daemon --serve
        """,
    )

    assert reading.sustained_cpu_cores == 0.3
    assert reading.transient_call_count == 1
    assert reading.process_count == 3
    assert reading.rss_gb == (1048576 + 2048 + 524288) / (1024 * 1024)
    assert reading.top == [
        (20.0, "/usr/local/bin/fno mux serve"),
        (10.0, "fno-agents-daemon --serve"),
    ]


def test_ac2_edge_malformed_line_is_counted_without_aborting() -> None:
    reading = parse_footprint(
        """\
        PID ELAPSED %CPU RSS COMMAND
        101 01:00:00 20.0 1024 fno daemon
        this is not a ps row
        102 01:00:00 nope 2048 fno broken-cpu
        """,
    )

    assert reading.process_count == 1
    assert reading.sustained_cpu_cores == 0.2
    assert reading.unparsed_lines == 2


def test_ac6_edge_claude_workers_do_not_count_as_fleet_overhead() -> None:
    reading = parse_footprint(
        """\
        PID ELAPSED %CPU RSS COMMAND
        201 02:00:00 300.0 1024 claude --worker
        202 02:00:00 12.5 1024 fno mux serve
        """,
    )

    assert reading.process_count == 1
    assert reading.sustained_cpu_cores == 0.125
    assert reading.top == [(12.5, "fno mux serve")]


def test_ac9_edge_high_cpu_burst_is_transient_not_sustained() -> None:
    reading = parse_footprint(
        """\
        PID ELAPSED %CPU RSS COMMAND
        301 00:00:01 92.0 1024 fno --version
        302 00:00:12 88.0 1024 fno-py agents list --json
        303 00:00:29 77.0 1024 fno-agents subscribe --json
        """,
    )

    assert reading.sustained_cpu_cores == 0.0
    assert reading.transient_call_count == 3
    assert reading.process_count == 3
    assert reading.top == []


def test_review_p2_python_launched_fno_and_agents_worker_are_attributed() -> None:
    reading = parse_footprint(
        """\
        PID ELAPSED %CPU RSS COMMAND
        401 01:00:00 12.0 1024 /opt/python/bin/python /opt/fno-py agents list --json
        402 01:00:00 18.0 2048 fno-agents-worker --stream
        """,
    )

    assert reading.process_count == 2
    assert reading.sustained_cpu_cores == 0.3
    assert reading.top == [
        (18.0, "fno-agents-worker --stream"),
        (12.0, "/opt/python/bin/python /opt/fno-py agents list --json"),
    ]


#: Command lines measured on one machine at 2026-09-04. A naive
#: `ps | grep -i pytest` reported four running tests here; two were real.
_REAL_CARGO_TEST = (
    "/Users/bb16/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo test "
    "--manifest-path crates/fno/Cargo.toml --lib portal_tests"
)
_LEAKED_KEEPER = (
    "/Users/bb16/.cargo/bin/fno-agents-worker --keeper --sock "
    "/private/var/folders/ch/T/fno-laneb-6nkmletp/mux/threads/wk-journey.sock "
    "--session wk-journey --cwd "
    "/private/var/folders/ch/T/pytest-of-bb16/pytest-313/test_lane_b_journey0"
)
_ZSH_WRAPPER = (
    "/bin/zsh -c -l eval 'cargo test --manifest-path crates/fno-agents/Cargo.toml "
    "--lib gc 2>&1 | tail -3'"
)


def test_ac3_hp_a_real_test_runner_is_counted() -> None:
    from fno.footprint import is_test_runner

    assert is_test_runner(_REAL_CARGO_TEST)
    assert is_test_runner("pytest cli/tests -k lanes")
    assert is_test_runner("/opt/homebrew/bin/py.test -x")
    assert is_test_runner("/usr/bin/python3.12 -m pytest cli/tests")
    assert is_test_runner("cargo nextest run --workspace")
    assert is_test_runner("fno doctor test cli/tests")
    assert is_test_runner("/opt/homebrew/bin/fno-py test")


def test_ac3_edge_a_pytest_path_in_the_command_line_is_not_a_test() -> None:
    """The trap this predicate closes, pinned to the measured command line: a
    leaked keeper whose SOCKET PATH sits under pytest-of-<user>. The substring
    is in the path, never in the program."""
    from fno.footprint import is_test_runner

    assert not is_test_runner(_LEAKED_KEEPER)
    assert not is_test_runner(_ZSH_WRAPPER)
    assert not is_test_runner("")
    assert not is_test_runner("cargo build --release")
    assert not is_test_runner("fno doctor lanes")
    assert not is_test_runner("vim test_pytest_helpers.py")


def test_ac3_hp_the_parser_counts_only_the_real_runners() -> None:
    """The positive control over one snapshot: four rows carry the word, two
    are tests. A count that read the whole command line would say four."""
    from fno.footprint import parse_footprint

    snapshot = "\n".join(
        [
            "PID PPID ELAPSED %CPU RSS COMMAND",
            f"100 1 01:00:00 10.0 1024 {_REAL_CARGO_TEST}",
            "101 1 01:00:00 10.0 1024 /usr/bin/python3 -m pytest cli/tests",
            f"102 1 01:00:00 10.0 1024 {_LEAKED_KEEPER}",
            f"103 1 01:00:00 10.0 1024 {_ZSH_WRAPPER}",
        ]
    )

    assert parse_footprint(snapshot).test_process_count == 2


def test_the_test_count_is_whole_machine_not_fleet_attributed() -> None:
    """A test a person started competes for the same box as a lane, so it is
    counted even though nothing attributes it to the fleet."""
    from fno.footprint import parse_footprint

    snapshot = (
        "PID PPID ELAPSED %CPU RSS COMMAND\n"
        "100 1 01:00:00 10.0 1024 pytest cli/tests\n"
    )
    reading = parse_footprint(snapshot)

    assert reading.process_count == 0
    assert reading.test_process_count == 1


def test_ac8_hp_top_consumers_name_the_process_and_the_worktree() -> None:
    """x-aeab AC8: 23 fno-py rows and 18 fno-agents-worker rows from one
    worktree aggregate to two consumers, the top named with its process
    count, the worktree named with how many of its rows run from it."""
    from fno.footprint import top_consumers

    fno_py = [
        (200.0 - float(i), f"/usr/local/bin/python3 fno-py doctor test {i}")
        for i in range(23)
    ]
    workers = [
        (90.0 - i, f"fno-agents-worker debug .fno/worktrees/x-b1ee/target/{i}")
        for i in range(18)
    ]
    sustained = fno_py + workers

    top = top_consumers(sustained)

    assert top[0]["name"] == "python3"
    assert top[0]["procs"] == 23
    assert top[1]["name"] == "fno-agents-worker"
    assert top[1]["procs"] == 18
    assert top[1]["worktree"] == ".fno/worktrees/x-b1ee"
    assert top[1]["worktree_procs"] == 18
    # Bounded to five consumers.
    assert len(top) <= 5


def test_ac9_edge_top_consumers_carry_no_worktree_when_argv_names_none() -> None:
    from fno.footprint import top_consumers

    top = top_consumers([(10.0, "/usr/sbin/cfprefsd agent")])

    assert top[0]["name"] == "cfprefsd"
    assert top[0]["worktree"] is None
    assert top[0]["worktree_procs"] == 0


#: The measured 2026-09-05 specimen: one wedged deps binary at ppid 1, its
#: dead children stacked as `<defunct>` rows against it.
_ORPHAN = "/Users/bb16/code/footnote/footnote/.claude/worktrees/preflight/crates/fno/target/debug/deps/fno-aa7282e99eecb046"


def _orphan_snapshot(orphan_ppid: int, orphan_path: str) -> str:
    """One orphan row + 227 defunct children + two live innocents."""
    header = "PID PPID ELAPSED %CPU RSS COMMAND"
    rows = [f"59929 {orphan_ppid} 03:07:00 0.0 4096 {orphan_path} portal"]
    rows += [f"{70000 + i} 59929 00:00:10 0.0 0 <defunct>" for i in range(227)]
    rows += [
        "900 1 01:00:00 0.1 1024 fno-agents-daemon --serve",
        "901 900 01:00:00 0.0 1024 cargo test -p fno",
    ]
    return "\n".join([header, *rows])


def test_orphan_test_binary_parser_names_the_pid_and_counts_its_zombies() -> None:
    from fno.footprint import parse_footprint

    reading = parse_footprint(_orphan_snapshot(1, _ORPHAN))

    assert len(reading.orphan_test_binaries) == 1
    orphan = reading.orphan_test_binaries[0]
    assert orphan.pid == 59929
    assert orphan.zombies == 227
    assert orphan.elapsed_seconds == 3 * 3600 + 7 * 60
    assert orphan.command == f"{_ORPHAN} portal"


def test_orphan_test_binary_parser_names_a_wedged_reaper_at_any_ppid() -> None:
    """The measured live case: a deps binary holding a zombie pile whose cargo
    parent was ALIVE (pipe-blocked). Zombie-count plus age is the predicate
    there, because ppid 1 alone misses exactly the one specimen observed."""
    from fno.footprint import parse_footprint

    reading = parse_footprint(_orphan_snapshot(4321, _ORPHAN))

    assert len(reading.orphan_test_binaries) == 1
    assert reading.orphan_test_binaries[0].pid == 59929
    assert reading.orphan_test_binaries[0].zombies == 227


def test_orphan_test_binary_parser_rejects_a_childless_deps_binary_with_a_parent() -> None:
    """A deps binary with a parent and no zombie pile is a LIVE lane: cargo is
    mid-run, the suite owns it, and naming it would misfire on healthy work."""
    from fno.footprint import parse_footprint

    header = "PID PPID ELAPSED %CPU RSS COMMAND"
    rows = [
        f"59929 4321 03:07:00 0.0 4096 {_ORPHAN} portal",
        "900 1 01:00:00 0.1 1024 fno-agents-daemon --serve",
    ]
    reading = parse_footprint("\n".join([header, *rows]))

    assert reading.orphan_test_binaries == ()


def test_orphan_test_binary_parser_holds_the_zombie_threshold_below_the_bar() -> None:
    """A handful of stragglers under a parented deps binary is not the wedge
    shape: under the bar and parented, nothing is named."""
    from fno.footprint import ORPHAN_MIN_ZOMBIES, parse_footprint

    header = "PID PPID ELAPSED %CPU RSS COMMAND"
    rows = [f"59929 4321 03:07:00 0.0 4096 {_ORPHAN} portal"]
    rows += [f"{70000 + i} 59929 00:00:10 0.0 0 <defunct>" for i in range(ORPHAN_MIN_ZOMBIES - 1)]
    reading = parse_footprint("\n".join([header, *rows]))

    assert reading.orphan_test_binaries == ()


def test_orphan_test_binary_parser_rejects_source_target_dir_shape() -> None:
    """`cli/src/fno/target` is a SOURCE dir: the path shape alone matched it,
    a sweep that trusted shapes alone deleted 66 real target dirs. The parser
    still passes the shape through (it cannot stat); the CONFIRMER is what
    rejects it - see the verb test with the same name."""
    from fno.footprint import parse_footprint

    source_shape = (
        "/Users/bb16/code/footnote/footnote/cli/src/fno/target/debug/deps/"
        "fno-aa7282e99eecb046"
    )
    reading = parse_footprint(_orphan_snapshot(1, source_shape))

    assert len(reading.orphan_test_binaries) == 1  # shape candidate only
    from fno import doctor_footprint

    assert doctor_footprint._confirmed_orphans(reading) == []


def test_orphan_test_binary_parser_counts_no_zombies_without_defunct_rows() -> None:
    from fno.footprint import parse_footprint

    reading = parse_footprint(
        "PID PPID ELAPSED %CPU RSS COMMAND\n"
        f"59929 1 03:07:00 0.0 4096 {_ORPHAN}\n"
    )

    assert reading.orphan_test_binaries[0].zombies == 0

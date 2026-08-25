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

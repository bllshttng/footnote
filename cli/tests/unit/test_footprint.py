"""Fixture-driven tests for the machine-footprint parser."""

from __future__ import annotations

from fno.footprint import parse_footprint


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

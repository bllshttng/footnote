"""The removed graph-export doctor surface stays absent."""

from __future__ import annotations

import inspect

from typer.testing import CliRunner

from fno import doctor
from fno.doctor_cli import doctor_app


def test_graph_export_command_is_not_registered() -> None:
    result = CliRunner().invoke(doctor_app, ["graph", "export", "--help"])
    assert result.exit_code != 0
    assert "No such command 'graph'" in result.output


def test_human_doctor_has_no_graph_export_stale_line() -> None:
    source = inspect.getsource(doctor.doctor_command) + inspect.getsource(doctor._emit_human)
    assert "graph_export" not in source
    assert "graph export STALE" not in source

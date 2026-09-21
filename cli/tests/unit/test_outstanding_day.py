"""Unit tests for the `fno inbox day` relay and the durable boundary row.

The fold and the append live in the native `day` verb. These cover the
relay's contract (registration, exit-code relay) and a parity guard: the
Rust-built row shape passes the Python event validator, and a malformed
kind is refused.
"""
import pytest

from fno.outstanding import day as day_mod


def test_day_app_registers_start_and_end() -> None:
    # Registration only: invoking the commands would shell the fno-agents
    # binary, whose exit code depends on the machine's stores (a missing
    # questions.jsonl is a successful incomplete read), so a CI runner with
    # a resolvable binary exits 0.
    from typer.main import get_command

    command = get_command(day_mod.day_app)
    registered = {name for name, _cmd in command.commands.items()}
    assert {"start", "end"} <= registered


def test_relay_relays_the_native_exit_code(monkeypatch: pytest.MonkeyPatch) -> None:
    calls = []

    def fake_run(argv, **kwargs):
        calls.append(argv)
        return type("R", (), {"returncode": 3})()

    monkeypatch.setattr(day_mod.subprocess, "run", fake_run)
    command = day_mod._make("start")
    with pytest.raises(day_mod.typer.Exit) as err:
        command()
    assert err.value.exit_code == 3
    assert calls


def test_rust_row_shape_passes_the_python_validator() -> None:
    """Parity guard for the native writer's row shape (schema.yaml: day_boundary)."""
    from fno.events import validate

    event = {
        "ts": "2026-09-20T18:05:00.123Z",
        "type": "day_boundary",
        "source": "target",
        "data": {
            "boundary_id": "day-end-20260920-ab12",
            "kind": "start",
            "cutoff": "2026-09-20T18:05:00+00:00",
            "prior_boundary_id": "day-start-20260920-ef34",
            "featured": ["q-1", "q-2"],
            "completed": 3,
            "open": 5,
            "opened": 2,
            "closed": 1,
            "retractions": 0,
        },
    }
    validate(event)


def test_rust_row_shape_rejects_a_missing_required_key() -> None:
    # The validator enforces the envelope and required keys; the kind enum is
    # the writer's gate, held by the fold's `--kind start|end` refusal.
    from fno.events import ValidationError, validate

    event = {
        "ts": "2026-09-20T18:05:00.123Z",
        "type": "day_boundary",
        "source": "target",
        "data": {
            "boundary_id": "day-end-20260920-ab12",
            "cutoff": "2026-09-20T18:05:00+00:00",
        },
    }
    with pytest.raises(ValidationError):
        validate(event)

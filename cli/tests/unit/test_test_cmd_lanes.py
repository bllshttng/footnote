"""`fno doctor test rust` derives its thread cap from the fno doctor lanes reading.

Every claim here is a positive marker: the flag in the built command, the
printed reading line. "The suite passed" proves nothing about the cap (the
x-e19e defect was exactly that a green run hid an 18-worker pile-up), so no
test in this file treats a zero-exit as evidence.
"""

from types import SimpleNamespace

from fno import test_cmd
from fno.doctor_lanes import LaneReading

MARKER_LOAD = 319.21
MARKER_CEILING = 96.0


def _fake_checkout(tmp_path, monkeypatch):
    """A repo-root-shaped tmp cwd with one crate to sweep."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("", encoding="utf-8")
    crate = tmp_path / "crates" / "alpha"
    crate.mkdir(parents=True)
    (crate / "Cargo.toml").write_text("[package]\n", encoding="utf-8")


def _capture_rust_cmds(monkeypatch, nextest=True):
    """Fake subprocess.run, fake nextest presence; returns the cmds list.

    subprocess and shutil are shared singletons, so these fakes intercept
    every subprocess call the reading path makes too - callers below patch
    the lane sensors themselves to keep the capture clean.
    """
    cmds: list[list[str]] = []

    class _Proc:
        returncode = 0

    monkeypatch.setattr(test_cmd.subprocess, "run", lambda cmd, env=None, **kw: cmds.append(list(cmd)) or _Proc())
    monkeypatch.setattr(
        test_cmd.shutil,
        "which",
        lambda name: "/x/cargo-nextest" if (nextest and name == "cargo-nextest") else None,
    )
    return cmds


def _reading(lane_count, refusal=""):
    reading = LaneReading()
    reading.lane_count = lane_count
    reading.refusal_reason = refusal
    return reading


def _reset_lanes(monkeypatch):
    monkeypatch.setattr(test_cmd, "_LANES_READING", None)


# ---------------------------------------------------------------------------
# THE MARKER: the full production wiring, not a stubbed reader.
# ---------------------------------------------------------------------------


def test_marker_breached_ceiling_caps_threads_at_one(tmp_path, monkeypatch, capsys):
    """Load 319.21 against a 96.0 ceiling (status exceeded) - the exact
    receipt from the x-e19e node - must both cap the threads at 1 and print
    the reading the cap was chosen from."""
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch)

    import fno.agents.court as court
    import fno.doctor_footprint as footprint
    import fno.doctor_lanes as lanes

    # subprocess is a shared singleton: anything real that shells out would
    # land in the capture. The court census is one such caller.
    monkeypatch.setattr(court, "gather_court", lambda rows=None: {})
    monkeypatch.setattr(
        footprint,
        "_spawn_load_snapshot",
        lambda: SimpleNamespace(
            load_1m=MARKER_LOAD, load_ceiling=MARKER_CEILING, spawn_load_status="exceeded"
        ),
    )
    monkeypatch.setattr(
        lanes,
        "read_macmon",
        lambda **kw: (
            {
                "cpu_usage_pct": 0.2,
                "memory": {
                    "ram_total": 32e9,
                    "ram_usage": 16e9,
                    "swap_total": 0,
                    "swap_usage": 0,
                },
                "sys_power": 5.0,
                "temp": {"cpu_temp_avg": 40.0},
            },
            None,
        ),
    )
    monkeypatch.setattr(lanes, "read_memory_pressure", lambda **kw: (0.5, None))
    monkeypatch.setattr(footprint, "cause_reading", lambda: (None, "test: no footprint"))
    monkeypatch.setattr(footprint, "live_registry_rows", lambda: ([], None))

    assert test_cmd._run_rust([]) == 0

    assert cmds, "no cargo command was built"
    cmd = cmds[0]
    assert cmd[:5] == ["cargo", "nextest", "run", "--test-threads", "1"]
    assert "--test-threads" not in cmd[5:]  # exactly one flag, in the base
    out = capsys.readouterr().out
    assert "0 more fit" in out
    assert "capped at 1" in out


# ---------------------------------------------------------------------------
# Derivation, override, refusal, failure - against faked readings.
# ---------------------------------------------------------------------------


def test_lanes_3_caps_nextest_at_three(tmp_path, monkeypatch, capsys):
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch)

    import fno.doctor_lanes as lanes

    monkeypatch.setattr(lanes, "read_lanes", lambda: _reading(3))
    assert test_cmd._run_rust([]) == 0
    assert cmds[0][:5] == ["cargo", "nextest", "run", "--test-threads", "3"]
    assert "3 more fit" in capsys.readouterr().out


def test_cargo_test_path_puts_flag_after_user_args(tmp_path, monkeypatch, capsys):
    """No nextest: the libtest flag rides behind `--`, which must come after
    every cargo-level arg the caller passed."""
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch, nextest=False)

    import fno.doctor_lanes as lanes

    monkeypatch.setattr(lanes, "read_lanes", lambda: _reading(3))
    assert test_cmd._run_rust(["--manifest-path", "crates/alpha/Cargo.toml"]) == 0
    assert cmds[0] == [
        "cargo",
        "test",
        "-q",
        "--manifest-path",
        "crates/alpha/Cargo.toml",
        "--",
        "--test-threads",
        "3",
    ]
    assert "capped at 3" in capsys.readouterr().out


def test_refused_reading_keeps_runner_default(tmp_path, monkeypatch, capsys):
    """A dark sensor is never headroom, but the suite must still run: no
    threads flag, and the note names the refusal."""
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch)

    import fno.doctor_lanes as lanes

    monkeypatch.setattr(lanes, "read_lanes", lambda: _reading(None, refusal="cpu arm dark"))
    assert test_cmd._run_rust([]) == 0
    assert all("--test-threads" not in cmd for cmd in cmds)
    out = capsys.readouterr().out
    assert "reading refused" in out
    assert "cpu arm dark" in out


def test_user_parallelism_flag_wins(tmp_path, monkeypatch, capsys):
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch)

    import fno.doctor_lanes as lanes

    monkeypatch.setattr(lanes, "read_lanes", lambda: _reading(64))
    assert test_cmd._run_rust(["-j4"]) == 0
    assert all("--test-threads" not in cmd for cmd in cmds)
    assert "user parallelism flag wins" in capsys.readouterr().out


def test_separator_counts_as_user_override(tmp_path, monkeypatch, capsys):
    """A caller's `--` means libtest args follow; injecting a second separator
    after it would hand our flag to the test binary as a literal."""
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch)

    import fno.doctor_lanes as lanes

    monkeypatch.setattr(lanes, "read_lanes", lambda: _reading(64))
    assert test_cmd._run_rust(["--", "--nocapture"]) == 0
    assert all("--test-threads" not in cmd for cmd in cmds)
    assert "user parallelism flag wins" in capsys.readouterr().out


def test_reading_failure_keeps_default_parallelism(tmp_path, monkeypatch, capsys):
    _fake_checkout(tmp_path, monkeypatch)
    _reset_lanes(monkeypatch)
    cmds = _capture_rust_cmds(monkeypatch)

    import fno.doctor_lanes as lanes

    def boom():
        raise RuntimeError("sensors exploded")

    monkeypatch.setattr(lanes, "read_lanes", boom)
    assert test_cmd._run_rust([]) == 0
    assert all("--test-threads" not in cmd for cmd in cmds)
    assert "reading failed (sensors exploded)" in capsys.readouterr().out


def test_reading_is_cached_per_process(monkeypatch):
    """The cache exists so the unit suite does not re-pay the sensor sample;
    prove it caches, and that a None cache is re-read (not sticky)."""
    _reset_lanes(monkeypatch)

    import fno.doctor_lanes as lanes

    calls = {"n": 0}

    def counting():
        calls["n"] += 1
        return _reading(7)

    monkeypatch.setattr(lanes, "read_lanes", counting)
    assert test_cmd._lanes_threads() == (7, "7 more fit (fno doctor lanes)")
    assert test_cmd._lanes_threads() == (7, "7 more fit (fno doctor lanes)")
    assert calls["n"] == 1  # second call served from the cache

    _reset_lanes(monkeypatch)  # a reset re-reads
    assert test_cmd._lanes_threads() == (7, "7 more fit (fno doctor lanes)")
    assert calls["n"] == 2

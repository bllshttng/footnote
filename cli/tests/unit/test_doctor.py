"""Unit tests for `fno doctor` (ab-5a1fc285 + ab-a78c9731).

Covers US1 (detection: fresh / stale / unknown / json / no-source / probe-error),
US3-adjacent --fix behavior (delegates to `fno update`, honors the IN_PROGRESS
guard), and US2 (rust staleness fold-in: full evidence mismatch -> stale,
partial evidence -> not stale, --fix rust-only leg runs the refresh helper,
never shells out to cargo directly).

The signal collectors (_resolve_source, _source_rev, _read_marker,
_probe_installed_verb, _rust_report, _read_rust_marker, _rust_source_rev,
_cargo_bin) are module-level so each test stubs them for a hermetic,
network-free verdict.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import doctor
from fno.cli import app

runner = CliRunner()


def _stub_signals(
    monkeypatch: pytest.MonkeyPatch,
    *,
    src: Path | None,
    source_rev: str | None,
    marker: str | None,
    capture_present: str,  # ProbeResult: "present" | "missing" | "unknown"
    rust_binary: str | None = None,
    rust_marker: str | None = None,
    rust_source_rev: str | None = None,
    cargo_bin_present: bool = False,
    deployed_config_keys: frozenset[str] | None = frozenset({"backlog.id_prefix"}),
    source_config_keys: frozenset[str] | None = frozenset({"backlog.id_prefix"}),
    content_drift: int | None = 0,
) -> None:
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: src)
    # Content-drift ground truth. Default 0 = "check ran, byte-identical" so a
    # fresh-marker test stays fresh; drift/indeterminate tests set >0 or None.
    # Never hashes the real installed package (hermetic).
    monkeypatch.setattr(doctor, "_python_content_drift", lambda source: content_drift)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: source_rev)
    monkeypatch.setattr(doctor, "_read_marker", lambda: marker)
    monkeypatch.setattr(doctor, "_probe_installed_verb", lambda: capture_present)
    # Config-schema surfaces (x-6c5b): default to EQUAL keysets so existing tests
    # exercise no drift; drift tests pass differing sets explicitly.
    monkeypatch.setattr(doctor, "_deployed_config_keys", lambda: deployed_config_keys)
    monkeypatch.setattr(doctor, "_source_config_keys", lambda source: source_config_keys)
    # Post ab-716cd330 `revision` carries the binary's self-reported crates/ rev
    # (the verdict driver), not the installed-rust-rev marker. `rust_marker` is
    # the value the resolved binary reports here.
    monkeypatch.setattr(
        doctor,
        "_rust_report",
        lambda: {"binary": rust_binary, "revision": rust_marker},
    )
    monkeypatch.setattr(doctor, "_read_rust_marker", lambda: rust_marker)
    monkeypatch.setattr(doctor, "_rust_source_rev", lambda source: rust_source_rev)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: cargo_bin_present)
    # Agent health (x-1c7b): default to a quiet, healthy machine. Left unstubbed
    # these shell out to the real `launchctl` and read the real claims root, so
    # every verdict test would inherit the developer's own dead agents.
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "ran", "hours": 3.0, "stale": False, "agent_installed": True},
    )
    monkeypatch.setattr(
        doctor, "_launch_agent_failures", lambda: {"applicable": True, "dead": []}
    )


# ---------------------------------------------------------------------------
# US1: detection
# ---------------------------------------------------------------------------


def test_ac1_hp_fresh_install_reports_healthy(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: marker == source HEAD => fresh, exit 0."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" in result.stdout


def test_ac1_err_missing_verb_reports_skew_and_exits_nonzero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-ERR: capability probe proves a missing verb => stale, exit nonzero, names the verb + remediation."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # marker even matches, but the probe wins
        capture_present="missing",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "missing: backlog capture" in result.stdout
    assert "fno update" in result.stdout


# ---------------------------------------------------------------------------
# x-3248 Change 5: per-harness surface freshness / dedupe
# ---------------------------------------------------------------------------

_MARKETPLACE_LIST_ONE = """\
MARKETPLACE             ROOT
openai-bundled          /Users/x/.codex/.tmp/bundled-marketplaces/openai-bundled
footnote-local          /Users/x/code/footnote/footnote
"""

_MARKETPLACE_LIST_DUP = """\
MARKETPLACE             ROOT
footnote-local          /Users/x/code/footnote/footnote
footnote                /Users/x/.codex/.tmp/footnote-clone
"""


def test_codex_marketplace_duplicates_pure_parser() -> None:
    # A single legitimate registration is not a duplicate; the header row and
    # foreign marketplaces never match.
    assert doctor._codex_marketplace_duplicates(_MARKETPLACE_LIST_ONE) == []
    assert doctor._codex_marketplace_duplicates("") == []
    # Two footnote rows -> both names reported.
    assert doctor._codex_marketplace_duplicates(_MARKETPLACE_LIST_DUP) == [
        "footnote-local",
        "footnote",
    ]


def test_ac5_doctor_names_codex_duplicate_and_dedupe_action(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC5: codex footnote registered twice -> doctor names the duplicate and
    the dedupe action instead of staying silent."""
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor, "_harness_surface_report",
        lambda: {"codex_marketplace_duplicates": ["footnote-local", "footnote"]},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "registered 2 times" in result.stdout
    assert "footnote-local, footnote" in result.stdout
    assert "codex plugin marketplace remove" in result.stdout


def test_doctor_reports_stale_opencode_plugin(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor, "_harness_surface_report", lambda: {"opencode": "stale"}
    )
    result = runner.invoke(app, ["doctor"])
    assert "opencode footnote plugin is STALE" in result.stdout
    assert "fno setup" in result.stdout


def test_doctor_main_run_points_at_codex_hooks_dual(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC4: a plain `fno doctor` (not just --codex-hooks) surfaces codex hooks
    dual-representation and points at the heal verb."""
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(
        doctor, "_harness_surface_report", lambda: {"codex_hooks_dual": True}
    )
    result = runner.invoke(app, ["doctor"])
    assert "codex hooks load from both" in result.stdout
    assert "--migrate-legacy-hooks-json" in result.stdout


def test_doctor_quiet_when_surfaces_healthy(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch, src=Path("/src"), source_rev="abc123", marker="abc123",
        capture_present="present",
    )
    monkeypatch.setattr(doctor, "_harness_surface_report", lambda: {})
    result = runner.invoke(app, ["doctor"])
    assert "opencode" not in result.stdout
    assert "marketplace" not in result.stdout


def test_ac1_err_rev_behind_reports_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR variant: marker behind source HEAD => stale, exit nonzero."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "behind" in result.stdout


def test_ac1_ui_json_is_single_object_on_stdout(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-UI: --json emits one JSON object on stdout; human text to stderr."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
    )
    result = runner.invoke(app, ["doctor", "--json"])
    assert result.exit_code != 0  # stale
    # stdout must be a single parseable JSON object with the contract fields.
    payload = json.loads(result.stdout.strip())
    assert payload["status"] == "stale"
    assert payload["python_stale"] is True
    assert payload["rust_stale"] is False
    assert payload["missing_verbs"] == []
    assert payload["source_rev"] == "newsha"
    assert payload["installed_rev"] == "oldsha"
    assert payload["rust_binary"] == "/cargo/bin/fno-agents"
    # Human/metadata text is on stderr, not mixed into the JSON stdout.
    assert "fno doctor" in result.stderr


def test_ac1_edge_no_source_degrades_to_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-EDGE: no resolvable source => unknown, exit 0 (cannot prove stale)."""
    _stub_signals(
        monkeypatch,
        src=None,
        source_rev=None,
        marker="abc123",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "no source checkout to compare against" in result.stdout


def test_ac1_fr_rev_probe_error_still_produces_verdict(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-FR: git rev undeterminable => revision signal degrades, verdict still produced, no traceback."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev=None,  # git rev-parse failed
        marker="abc123",
        capture_present="present",  # capability probe still ran and found nothing missing
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert result.exception is None
    assert "unknown" in result.stdout


def test_marker_absent_is_not_false_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    """Boundary: a missing marker must not report 'fresh' even with source resolvable."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker=None,  # pre-marker install
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" not in result.stdout
    assert "unknown" in result.stdout


def test_rust_binary_always_reported(monkeypatch: pytest.MonkeyPatch) -> None:
    """The resolved fno-agents binary is always reported; 'undeterminable' is NOT pinned."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/wheel/_bin/fno-agents",
    )
    result = runner.invoke(app, ["doctor"])
    assert "/wheel/_bin/fno-agents" in result.stdout


# ---------------------------------------------------------------------------
# Python config-schema drift
# ---------------------------------------------------------------------------


def test_config_schema_drift_reports_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    """Deployed FIELD_META missing a key the source defines => stale, exit nonzero,
    names a missing key + remediation. Revs match, so this is caught by the schema
    fingerprint alone (the exact stale-uv-tool symptom)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # rev + verb both look fresh...
        capture_present="present",
        deployed_config_keys=frozenset({"project.id"}),  # ...but the config schema is behind
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "config schema is STALE" in result.stdout
    assert "backlog.id_prefix" in result.stdout
    assert "fno update" in result.stdout


def test_config_schema_drift_shows_rev_delta_when_also_rev_behind(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Overlap case (schema-behind AND rev-behind): the config message leads but the
    rev delta is still surfaced, not dropped."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",  # rev-behind too
        capture_present="present",
        deployed_config_keys=frozenset({"project.id"}),
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "config schema is STALE" in result.stdout
    assert "oldsha" in result.stdout and "newsha" in result.stdout


def test_config_schema_in_sync_is_silent(monkeypatch: pytest.MonkeyPatch) -> None:
    """Matching keysets on an otherwise-fresh install stay silent (no false positive)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        deployed_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" in result.stdout
    assert "config schema" not in result.stdout


def test_config_deployed_ahead_of_source_not_stale(monkeypatch: pytest.MonkeyPatch) -> None:
    """A deployed CLI with MORE keys than source is not drift (don't cry wolf)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        deployed_config_keys=frozenset({"project.id", "backlog.id_prefix", "new.key"}),
        source_config_keys=frozenset({"project.id", "backlog.id_prefix"}),
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "config schema is STALE" not in result.stdout


_FLAT_REGISTRY = (
    "from dataclasses import dataclass\n"
    "@dataclass\n"
    "class Meta:\n"
    "    doc: str\n"
    'FIELD_META: dict[str, Meta] = {\n'
    '    "project.id": Meta("x"),\n'
    '    "backlog.id_prefix": Meta("y"),\n'
    "}\n"
)


def test_parse_field_meta_keys_flat() -> None:
    """A flat literal of constant string keys parses to the exact keyset."""
    assert doctor._parse_field_meta_keys(_FLAT_REGISTRY) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )


def test_parse_field_meta_keys_spread_returns_none() -> None:
    """A `**spread` (or computed key) can't be read completely => None, never a
    truncated set that would risk a false 'fresh'."""
    spread = 'BASE = {"a.b": 1}\nFIELD_META = {**BASE, "backlog.id_prefix": 2}\n'
    assert doctor._parse_field_meta_keys(spread) is None
    computed = 'K = "x"\nFIELD_META = {K: 1}\n'
    assert doctor._parse_field_meta_keys(computed) is None


def test_parse_field_meta_keys_split_annotation_then_assign() -> None:
    """A bare annotation followed by a separate dict assignment still parses: the
    valueless AnnAssign is skipped, not treated as an unreadable form."""
    split = (
        "FIELD_META: dict[str, int]\n"
        'FIELD_META = {"project.id": 1, "backlog.id_prefix": 2}\n'
    )
    assert doctor._parse_field_meta_keys(split) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )


def test_parse_field_meta_keys_broken_or_absent_returns_none() -> None:
    """Unparseable text or no FIELD_META => None."""
    assert doctor._parse_field_meta_keys("FIELD_META = {  # truncated\n") is None
    assert doctor._parse_field_meta_keys("x = 1\n") is None


def _init_git_source(root: Path, registry_text: str) -> None:
    """Commit a registry.py into a throwaway git repo laid out like the cli source."""
    import subprocess

    reg = root / "src" / "fno" / "config" / "registry.py"
    reg.parent.mkdir(parents=True)
    reg.write_text(registry_text, encoding="utf-8")
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@t",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@t",
    }
    for cmd in (["init", "-q"], ["add", "-A"], ["commit", "-qm", "init"]):
        subprocess.run(["git", "-C", str(root), *cmd], check=True, env=env)


def test_source_config_keys_reads_committed_head(tmp_path: Path) -> None:
    """The source keyset comes from committed HEAD, NOT the dirty working tree: an
    uncommitted edit must not leak into the verdict (matches _source_rev semantics)."""
    _init_git_source(tmp_path, _FLAT_REGISTRY)
    assert doctor._source_config_keys(tmp_path) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )
    # Add a key in the WORKING TREE only (no commit). HEAD is unchanged, so the
    # committed keyset must not include it - else a dirty checkout false-STALEs.
    reg = tmp_path / "src" / "fno" / "config" / "registry.py"
    reg.write_text(
        _FLAT_REGISTRY.replace("}\n", '    "batch.enabled": Meta("z"),\n}\n'),
        encoding="utf-8",
    )
    assert doctor._source_config_keys(tmp_path) == frozenset(
        {"project.id", "backlog.id_prefix"}
    )


def test_source_config_keys_fails_open_on_missing_or_non_git(tmp_path: Path) -> None:
    """None source or a non-git dir => None (skip the check, never crash doctor)."""
    assert doctor._source_config_keys(None) is None
    assert doctor._source_config_keys(tmp_path) is None  # not a git repo


def test_deployed_config_keys_reflects_real_field_meta() -> None:
    """The deployed surface is the real in-process FIELD_META (includes the sentinel key)."""
    keys = doctor._deployed_config_keys()
    assert keys is not None
    assert "backlog.id_prefix" in keys


# ---------------------------------------------------------------------------
# _verdict: pure decision matrix
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "kwargs,expected_status,expected_python_stale,expected_rust_stale",
    [
        # Existing rows: no rust evidence passed - rust_stale must stay False.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present"), "fresh", False, False),
        (dict(source_resolved=True, source_rev="a", marker="b", capture_present="present"), "stale", True, False),
        (dict(source_resolved=True, source_rev="a", marker=None, capture_present="present"), "unknown", False, False),
        (dict(source_resolved=False, source_rev=None, marker="a", capture_present="present"), "unknown", False, False),
        (dict(source_resolved=True, source_rev=None, marker="a", capture_present="present"), "unknown", False, False),
        # Probe proves missing verb => stale regardless of source/marker.
        (dict(source_resolved=False, source_rev=None, marker=None, capture_present="missing"), "stale", True, False),
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="missing"), "stale", True, False),
        # Rust fold-in: full evidence mismatch + python fresh -> rust_stale True, status stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="bbb", cargo_bin_present=True), "stale", False, True),
        # Rust fold-in: full evidence match + python fresh -> rust_stale False, status fresh.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="aaa", cargo_bin_present=True), "fresh", False, False),
        # Rust fold-in: partial evidence (no cargo bin) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="bbb", cargo_bin_present=False), "fresh", False, False),
        # Rust fold-in: partial evidence (marker None) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev=None, rust_source_rev="bbb", cargo_bin_present=True), "fresh", False, False),
        # Rust fold-in: partial evidence (rust_source_rev None) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev=None, cargo_bin_present=True), "fresh", False, False),
        # Rust fold-in: python stale + rust stale -> status stale.
        (dict(source_resolved=True, source_rev="a", marker="b", capture_present="present",
              rust_installed_rev="aaa", rust_source_rev="bbb", cargo_bin_present=True), "stale", True, True),
        # Config drift: source defines a key deployed lacks, revs match -> python stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              deployed_config_keys=frozenset({"a"}), source_config_keys=frozenset({"a", "b"})),
         "stale", True, False),
        # Config drift proven even when the python status would otherwise be unknown.
        (dict(source_resolved=True, source_rev="a", marker=None, capture_present="present",
              deployed_config_keys=frozenset({"a"}), source_config_keys=frozenset({"a", "b"})),
         "stale", True, False),
        # Config match -> no drift, stays fresh.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              deployed_config_keys=frozenset({"a", "b"}), source_config_keys=frozenset({"a", "b"})),
         "fresh", False, False),
        # Config partial evidence (source keyset unknown) -> not stale.
        (dict(source_resolved=True, source_rev="a", marker="a", capture_present="present",
              deployed_config_keys=frozenset({"a"}), source_config_keys=None), "fresh", False, False),
    ],
)
def test_verdict_matrix(kwargs, expected_status, expected_python_stale, expected_rust_stale) -> None:
    v = doctor._verdict(**kwargs)
    assert v["status"] == expected_status
    assert v["python_stale"] is expected_python_stale
    assert v["rust_stale"] is expected_rust_stale


# ---------------------------------------------------------------------------
# AC2: rust staleness detection
# ---------------------------------------------------------------------------


def test_ac2_hp_rust_stale_json_payload(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC2-HP: cargo bin + marker aaa + rust_source_rev bbb + python fresh -> --json shows rust_stale: true, status stale, exit 1."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    result = runner.invoke(app, ["doctor", "--json"])
    assert result.exit_code == 1
    payload = json.loads(result.stdout.strip())
    assert payload["rust_stale"] is True
    assert payload["rust_installed_rev"] == "aaa"
    assert payload["rust_source_rev"] == "bbb"
    assert payload["status"] == "stale"
    assert payload["python_stale"] is False


@pytest.mark.parametrize(
    "rust_marker,rust_source_rev,cargo_bin_present,python_status,expected_exit",
    [
        # No cargo bin - not stale.
        (None, "bbb", False, "fresh", 0),
        # Marker None - not stale (unknown).
        (None, "bbb", True, "fresh", 0),
        # rust_source_rev None - not stale.
        ("aaa", None, True, "fresh", 0),
        # python unknown + rust evidence gap -> still exit 0 unknown.
        (None, None, False, "unknown", 0),
    ],
)
def test_ac2_err_degrade_matrix(
    monkeypatch: pytest.MonkeyPatch,
    rust_marker: str | None,
    rust_source_rev: str | None,
    cargo_bin_present: bool,
    python_status: str,
    expected_exit: int,
) -> None:
    """AC2-ERR: incomplete rust evidence -> rust_stale false, exit 0."""
    # For python_status "unknown" use no source, for "fresh" use matching marker.
    if python_status == "unknown":
        src = None
        source_rev = None
        marker = None
        cp = "present"
    else:
        src = Path("/src")
        source_rev = "abc"
        marker = "abc"
        cp = "present"
    _stub_signals(
        monkeypatch,
        src=src,
        source_rev=source_rev,
        marker=marker,
        capture_present=cp,
        rust_marker=rust_marker,
        rust_source_rev=rust_source_rev,
        cargo_bin_present=cargo_bin_present,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == expected_exit
    payload_json_result = runner.invoke(app, ["doctor", "--json"])
    payload = json.loads(payload_json_result.stdout.strip())
    assert payload["rust_stale"] is False


def test_ac2_err_binary_present_marker_absent_explains(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC2-ERR: binary present but marker absent -> human output explains why revision is unknown."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker=None,  # no marker yet
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    # Should mention revision unknown / no marker / seed it via fno update.
    combined = result.stdout + result.stderr
    assert "revision unknown" in combined or "no installed-rust-rev marker" in combined


@pytest.mark.parametrize(
    "rust_marker,rust_source_rev,rust_binary,cargo_bin_present,expected_fragment",
    [
        # Not installed.
        (None, None, None, False, "not found"),
        # Fresh.
        ("aaa", "aaa", "/cargo/bin/fno-agents", True, "fresh"),
        # Stale.
        ("aaa", "bbb", "/cargo/bin/fno-agents", True, "STALE"),
        # Unknown: binary present, marker absent.
        (None, "bbb", "/cargo/bin/fno-agents", True, "unknown"),
    ],
)
def test_ac2_ui_rust_human_line_states(
    monkeypatch: pytest.MonkeyPatch,
    rust_marker: str | None,
    rust_source_rev: str | None,
    rust_binary: str | None,
    cargo_bin_present: bool,
    expected_fragment: str,
) -> None:
    """AC2-UI: four rust human-output states each produce their identifying line."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
        rust_binary=rust_binary,
        rust_marker=rust_marker,
        rust_source_rev=rust_source_rev,
        cargo_bin_present=cargo_bin_present,
    )
    result = runner.invoke(app, ["doctor"])
    combined = result.stdout + result.stderr
    assert expected_fragment in combined


# ---------------------------------------------------------------------------
# AC2-EDGE: --fix routing
# ---------------------------------------------------------------------------


def test_ac2_edge_rust_only_stale_fix_calls_refresh_not_update(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (a): rust-only stale + --fix -> refresh_rust_bins called, update_command NOT called, exit 0."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Fix C2: ensure the IN_PROGRESS guard does not fire so the fix proceeds
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    refresh_calls: list[dict] = []
    update_calls: list[dict] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append({"source": source, "force": force, "dry_run": dry_run})
        return "refreshed"

    def _fake_update(source=None, dry_run=False, force=False):
        update_calls.append({"source": source})

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert len(refresh_calls) == 1
    assert refresh_calls[0]["source"] == Path("/src")
    assert len(update_calls) == 0


def test_ac2_edge_rust_only_fix_fresh_outcome_exits_zero(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (PR #438 Gemini): a concurrent refresh can land between the
    verdict read and the repair, making the helper return 'fresh'. The goal
    state is achieved, so --fix exits 0 - never a failure."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Fix C2: ensure the IN_PROGRESS guard does not fire so the fix proceeds
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    def _fake_refresh(source, *, force=False, dry_run=False):
        return "fresh"

    def _fake_update(source=None, dry_run=False, force=False):
        raise AssertionError("update_command must not run for rust-only --fix")

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert "already fresh" in result.stderr


def test_ac2_edge_rust_only_fix_no_marker_outcome_exits_one(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """ab-703f2ed2: cargo succeeded but the marker was not written (e.g.
    ~/.fno unwritable). The stale verdict cannot converge - the next
    doctor run still reports rust stale - so --fix must not claim success."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    def _fake_refresh(source, *, force=False, dry_run=False):
        return "refreshed-no-marker"

    def _fake_update(source=None, dry_run=False, force=False):
        raise AssertionError("update_command must not run for rust-only --fix")

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 1
    assert "will not converge" in result.stderr


def test_ac2_edge_python_and_rust_stale_fix_delegates_update_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (b): python stale + rust stale -> delegates to update_command, _refresh_rust_bins NOT called directly."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    refresh_calls: list[dict] = []
    update_calls: list[dict] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append({})
        return "refreshed"

    def _fake_update(source=None, dry_run=False, force=False):
        update_calls.append({"source": source})

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exception is None
    assert len(update_calls) == 1
    assert len(refresh_calls) == 0  # doctor did not call it directly; update_command owns it


def test_ac2_edge_fix_json_rust_stale_no_repair(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-EDGE (c): --fix --json with rust-only stale -> no repair call, JSON on stdout, skip message on stderr."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    refresh_calls: list[dict] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append({})
        return "refreshed"

    def _fake_update(source=None, dry_run=False, force=False):
        pass

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)
    monkeypatch.setattr(update, "update_command", _fake_update)

    result = runner.invoke(app, ["doctor", "--json", "--fix"])
    # stdout is still a single parseable JSON object.
    payload = json.loads(result.stdout.strip())
    assert payload["status"] == "stale"
    assert payload["rust_stale"] is True
    assert len(refresh_calls) == 0
    # The skip message appears on stderr.
    assert "--fix skipped under --json" in result.stderr


# ---------------------------------------------------------------------------
# AC2-FR: follow-up fresh run after successful rust-only fix
# ---------------------------------------------------------------------------


def test_ac2_fr_rust_only_fix_exits_zero_and_followup_is_fresh(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-FR: successful rust-only fix exits 0; re-run with matching markers -> fresh exit 0."""
    # First run: rust only stale.
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Fix C2: ensure the IN_PROGRESS guard does not fire
    monkeypatch.setattr(update, "_target_in_progress", lambda: False)

    def _fake_refresh(source, *, force=False, dry_run=False):
        return "refreshed"

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0

    # Follow-up run: markers match -> fresh.
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="bbb",  # after fix, marker == source rev
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    result2 = runner.invoke(app, ["doctor"])
    assert result2.exit_code == 0
    combined = result2.stdout + result2.stderr
    assert "fresh" in combined


# ---------------------------------------------------------------------------
# --fix
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# x-8c3b: --fix acts on a dead pr-watch verdict (advisory, never flips exit)
# ---------------------------------------------------------------------------


def _dead_pr_watch(monkeypatch) -> None:
    monkeypatch.setattr(
        doctor,
        "_pr_watch_liveness",
        lambda: {
            "enabled": True, "verdict": "dead", "detail": "no tick recorded",
            "fix": "fno pr-watch install", "loaded": True, "last_tick": None,
        },
    )


def test_fix_heals_dead_pr_watch_on_fresh_binary(monkeypatch: pytest.MonkeyPatch) -> None:
    """A fresh binary with a dead watcher still heals it under --fix, exit 0."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _dead_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    heal_calls: list = []
    monkeypatch.setattr(pw, "heal_watcher", lambda **kw: heal_calls.append(kw) or ("bounced x", 0))

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0  # advisory: a dead watcher never flips the exit
    assert len(heal_calls) == 1
    assert "pr-watch heal" in result.stderr


def test_fix_json_skips_pr_watch_heal(monkeypatch: pytest.MonkeyPatch) -> None:
    """--json preserves the single-JSON-object stdout contract: no heal side-effect."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _dead_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    monkeypatch.setattr(pw, "heal_watcher", lambda **kw: pytest.fail("must not heal under --json"))

    result = runner.invoke(app, ["doctor", "--json", "--fix"])
    assert result.exit_code == 0
    # stdout is exactly one JSON object.
    json.loads(result.stdout.strip())


def test_no_fix_never_heals(monkeypatch: pytest.MonkeyPatch) -> None:
    """A plain `doctor` (no --fix) reports but never runs the bounce."""
    _stub_signals(monkeypatch, src=Path("/src"), source_rev="abc", marker="abc",
                  capture_present="present")
    _dead_pr_watch(monkeypatch)
    import fno.pr_watch._install as pw
    monkeypatch.setattr(pw, "heal_watcher", lambda **kw: pytest.fail("must not heal without --fix"))

    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "pr-watch enabled but not running" in result.stdout


def test_ac3_hp_fix_delegates_to_update(monkeypatch: pytest.MonkeyPatch) -> None:
    """AC3-HP: --fix on a stale Python install delegates to `fno update`."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",
    )
    calls: dict[str, object] = {}

    from fno import update

    def _fake_update(source=None, dry_run=False, force=False):  # noqa: ANN001
        calls["source"] = source
        calls["called"] = True

    monkeypatch.setattr(update, "update_command", _fake_update)
    result = runner.invoke(app, ["doctor", "--fix", "--source", "/src"])
    assert calls.get("called") is True
    assert str(calls["source"]) == "/src"
    assert result.exception is None


def test_ac3_edge_fix_respects_in_progress_guard(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC3-EDGE: --fix during an IN_PROGRESS target refuses (the update guard fires)."""
    # Stale via probe so --fix reaches the python_stale branch without needing source.
    _stub_signals(
        monkeypatch,
        src=None,
        source_rev=None,
        marker=None,
        capture_present="missing",
    )
    # An IN_PROGRESS target-state.md in the resolved repo root triggers update's guard.
    repo_root = tmp_path / "repo"
    (repo_root / ".fno").mkdir(parents=True)
    (repo_root / ".fno" / "target-state.md").write_text(
        "---\nstatus: IN_PROGRESS\n---\n", encoding="utf-8"
    )
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    # Defensive: even if the guard were bypassed, never actually install.
    import fno.update as update_mod

    monkeypatch.setattr(update_mod.os, "execvp", lambda *a, **kw: None)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 1
    assert "refused" in (result.stderr + result.stdout)


def test_fix_nothing_to_do_when_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    """--fix on a fresh install reports nothing to fix and exits 0."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
    )
    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert "nothing to fix" in result.stderr


def test_stale_missing_verb_without_source_says_behind_source_not_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Gemini review: a missing-verb stale verdict with no resolved source must
    print 'behind source', never 'behind None'."""
    _stub_signals(
        monkeypatch,
        src=None,            # no source resolved
        source_rev=None,
        marker=None,
        capture_present="missing",  # probe proves stale regardless of source
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code != 0
    assert "behind None" not in result.stdout
    assert "behind source" in result.stdout
    assert "missing: backlog capture" in result.stdout


def test_json_fix_does_not_pollute_stdout_or_delegate(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Codex review: `--json --fix` on a stale install keeps stdout a single JSON
    object and does NOT delegate to `fno update` (which prints to stdout)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="newsha",
        marker="oldsha",
        capture_present="present",  # rev-mismatch stale
    )
    from fno import update

    called = {"update": False}

    def _fake_update(source=None, dry_run=False, force=False):  # noqa: ANN001
        called["update"] = True

    monkeypatch.setattr(update, "update_command", _fake_update)
    result = runner.invoke(app, ["doctor", "--json", "--fix"])
    assert result.exit_code != 0  # stale
    # stdout is still a single parseable JSON object - no update chatter.
    payload = json.loads(result.stdout.strip())
    assert payload["status"] == "stale"
    # update was NOT executed under --json (would have polluted stdout).
    assert called["update"] is False
    # The skip is explicit, on stderr.
    assert "--fix skipped under --json" in result.stderr


# ---------------------------------------------------------------------------
# Fix C2: doctor --fix rust-only branch honors the IN_PROGRESS guard
# ---------------------------------------------------------------------------


def test_ac2_edge_rust_only_fix_respects_in_progress_guard(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Fix C2: rust-only stale + --fix + IN_PROGRESS -> exit 1, "refused" in stderr,
    _refresh_rust_bins never called.

    The python_stale delegation path already inherits update's own guard. The
    rust-only branch called _refresh_rust_bins directly, bypassing the guard.
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )
    from fno import update

    # Simulate IN_PROGRESS
    monkeypatch.setattr(update, "_target_in_progress", lambda: True)

    # Tripwire: _refresh_rust_bins must NOT be called
    def _tripwire_refresh(source, *, force=False, dry_run=False):
        raise AssertionError("_refresh_rust_bins must not be called when IN_PROGRESS")

    monkeypatch.setattr(update, "_refresh_rust_bins", _tripwire_refresh)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 1
    assert "refused" in result.stderr
    assert "IN_PROGRESS" in result.stderr


# ---------------------------------------------------------------------------
# Fix C1: _emit_human never prints STALE for non-cargo binaries
# ---------------------------------------------------------------------------


def test_ac2_ui_non_cargo_binary_never_reports_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Fix C1: a bundled-wheel/PATH binary with a leftover marker mismatch must
    NOT print STALE. The JSON verdict has rust_stale: false (no cargo bin), so
    human output must not contradict it.

    cargo_bin_present=False + marker mismatch -> exit 0, output contains
    "not tracked", does NOT contain "STALE".
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="xyz",
        marker="xyz",
        capture_present="present",
        rust_binary="/wheel/_bin/fno-agents",
        rust_marker="aaa",       # mismatch evidence but unproven
        rust_source_rev="bbb",
        cargo_bin_present=False,  # no cargo bin -> rust_stale is False in verdict
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0, f"exit code {result.exit_code}, output: {result.stdout}{result.stderr}"
    combined = result.stdout + result.stderr
    assert "STALE" not in combined, (
        f"Non-cargo binary must never produce STALE output. Got:\n{combined}"
    )
    assert "not tracked" in combined, (
        f"Expected 'not tracked' for non-cargo binary. Got:\n{combined}"
    )


def test_ac3_fr_fix_rust_only_stale_runs_refresh_never_raw_cargo(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC3-FR (new contract): rust-only stale --fix RUNS the refresh helper, never invokes
    cargo via raw subprocess directly from doctor.py.

    Tripwire: doctor.subprocess.run is wired to explode; only the helper (which is
    stubbed separately) may be called.
    """
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc",
        marker="abc",
        capture_present="present",
        rust_binary="/cargo/bin/fno-agents",
        rust_marker="aaa",
        rust_source_rev="bbb",
        cargo_bin_present=True,
    )

    # Tripwire: doctor must never call subprocess.run (which would mean raw cargo).
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("doctor.py must never run cargo directly")),
    )
    # The advisory mux front-door probe (`fno mux ls`) is a legit doctor
    # subprocess, unrelated to the raw-cargo concern this tripwire guards; stub it
    # so the tripwire isolates cargo, not the probe.
    monkeypatch.setattr(doctor, "_probe_is_mux", lambda p: False)
    # Same reasoning for the per-harness surface probe (`codex plugin marketplace
    # list`) - a legit advisory subprocess, not raw cargo.
    monkeypatch.setattr(doctor, "_harness_surface_report", lambda: {})

    from fno import update

    refresh_calls: list[str] = []

    def _fake_refresh(source, *, force=False, dry_run=False):
        refresh_calls.append(str(source))
        return "refreshed"

    monkeypatch.setattr(update, "_refresh_rust_bins", _fake_refresh)

    result = runner.invoke(app, ["doctor", "--fix"])
    assert result.exit_code == 0
    assert len(refresh_calls) == 1
    assert refresh_calls[0] == str(Path("/src"))


# ---------------------------------------------------------------------------
# ab-24a59d50: binary self-reported git rev (build.rs embed)
# ---------------------------------------------------------------------------


def _fake_run(returncode: int, stdout: str):
    """Build a subprocess.run stub returning a fixed CompletedProcess."""
    import subprocess

    def _run(cmd, *args, **kwargs):
        return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")

    return _run


def test_binary_self_rev_returns_git_rev(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        doctor.subprocess, "run", _fake_run(0, '{"git_rev": "deadbeefcafe", "package": "0.1.0"}')
    )
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") == "deadbeefcafe"


def test_binary_self_rev_none_for_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    # A non-git build self-reports "unknown"; treat that as no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, '{"git_rev": "unknown"}'))
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_on_nonzero_exit(monkeypatch: pytest.MonkeyPatch) -> None:
    # An old binary lacking the `version` verb exits non-zero -> no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(2, ""))
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_on_malformed_json(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, "not json at all"))
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_on_oserror(monkeypatch: pytest.MonkeyPatch) -> None:
    def _boom(*args, **kwargs):
        raise OSError("no such binary")

    monkeypatch.setattr(doctor.subprocess, "run", _boom)
    assert doctor._binary_self_rev("/cargo/bin/fno-agents") is None


def test_binary_self_rev_none_when_no_binary() -> None:
    # Skips the subprocess entirely when there is no resolved binary.
    assert doctor._binary_self_rev(None) is None


def test_rust_report_revision_from_cargo_binary_single_spawn(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # ab-716cd330: verdict `revision` = binary's crates/ subtree rev (marker-free).
    # gemini PR #491: when resolved == cargo binary, probe `version --json` ONCE.
    from fno.agents import rust_runtime

    monkeypatch.setattr(
        rust_runtime, "resolve_installed_binary", lambda: Path("/cargo/bin/fno-agents")
    )
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "/cargo/bin/fno-agents")
    calls: list[str | None] = []

    def fake_version_json(binary: str | None) -> dict:
        calls.append(binary)
        return {"git_rev": "feedface1234", "crates_rev": "cab5cab5cab5"}

    monkeypatch.setattr(doctor, "_binary_version_json", fake_version_json)
    report = doctor._rust_report()
    assert report["binary"] == "/cargo/bin/fno-agents"
    assert report["revision"] == "cab5cab5cab5"  # verdict driver = cargo crates rev
    assert report["binary_rev"] == "feedface1234"  # HEAD identity, informational
    assert calls == ["/cargo/bin/fno-agents"]  # single spawn when paths coincide


def test_rust_report_revision_comes_from_cargo_not_resolved(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # codex PR #491: a bundled sibling resolves, but the verdict rev must come
    # from the cargo binary the gate (_cargo_bin_present) + --fix target.
    from fno.agents import rust_runtime

    monkeypatch.setattr(
        rust_runtime, "resolve_installed_binary", lambda: Path("/bundled/fno-agents")
    )
    monkeypatch.setattr(doctor, "_cargo_bin_path", lambda: "/cargo/bin/fno-agents")

    def fake_version_json(binary: str | None) -> dict:
        if binary == "/bundled/fno-agents":
            return {"git_rev": "bbbbbbbbbbbb", "crates_rev": "bundledcrates"}
        return {"git_rev": "cccccccccccc", "crates_rev": "cargocrates12"}

    monkeypatch.setattr(doctor, "_binary_version_json", fake_version_json)
    report = doctor._rust_report()
    assert report["binary"] == "/bundled/fno-agents"  # display = resolved binary
    assert report["binary_rev"] == "bbbbbbbbbbbb"  # informational = resolved HEAD
    assert report["revision"] == "cargocrates12"  # verdict = CARGO crates rev


# ---------------------------------------------------------------------------
# ab-716cd330: binary self-reported crates/ subtree rev (build.rs embed)
# ---------------------------------------------------------------------------


def test_binary_crates_rev_returns_crates_rev(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        _fake_run(0, '{"git_rev": "deadbeefcafe", "crates_rev": "cab5cab5cab5"}'),
    )
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") == "cab5cab5cab5"


def test_binary_crates_rev_none_for_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    # A non-git build self-reports "unknown"; treat that as no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, '{"crates_rev": "unknown"}'))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_when_field_absent(monkeypatch: pytest.MonkeyPatch) -> None:
    # A pre-ab-716cd330 binary has git_rev but no crates_rev -> no signal.
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, '{"git_rev": "deadbeefcafe"}'))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_on_nonzero_exit(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(2, ""))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_on_malformed_json(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(doctor.subprocess, "run", _fake_run(0, "not json at all"))
    assert doctor._binary_crates_rev("/cargo/bin/fno-agents") is None


def test_binary_crates_rev_none_when_no_binary() -> None:
    assert doctor._binary_crates_rev(None) is None


def test_emit_human_binary_rev_shown_as_build_provenance(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """AC1-UI + AC1-EDGE: git_rev (HEAD) is build provenance only, never
    compared to the crates/ source rev. A "fresh" verdict and the HEAD line must
    never contradict each other - even when HEAD has advanced past the last
    crates/ commit (python-only commits since), the exact false alarm from the
    incident where "rust bins fresh" printed beside a bogus rev mismatch.
    """
    result = {
        "status": "fresh",
        "rust_stale": False,
        "rust_installed_rev": "abc123abc123",  # crates_rev drives the verdict
        "rust_source_rev": "abc123abc123",  # crates/ subtree rev
        "missing_verbs": [],
        "python_stale": False,
    }
    # binary_rev = HEAD, DELIBERATELY newer than the crates/ rev (python-only commits).
    rust = {
        "binary": "/cargo/bin/fno-agents",
        "revision": "abc123abc123",
        "binary_rev": "deadbeef9999",
    }
    doctor._emit_human(result, Path("/src"), rust, err=False, cargo_present=True)
    out = capsys.readouterr().out
    # Fresh verdict AND the HEAD line coexist without contradiction.
    assert "rust bins fresh" in out
    assert "built at HEAD deadbeef9999" in out
    assert "build provenance" in out
    # The retired apples-to-oranges framing must be gone.
    assert "source crates/ rev" not in out
    assert "self-reports rev" not in out


# --- x-c267: mux front-door health (advisory) ---


@pytest.mark.parametrize(
    "mux, which_fno, probe, expected",
    [
        (None, None, False, "not-installed"),
        # `fno` on PATH but not a mux (probe says no) + no cargo mux -> not-installed
        (None, "/home/x/.local/bin/fno", False, "not-installed"),
        # custom --root mux: not at $CARGO_HOME/bin, but `fno` on PATH answers the
        # mux verb -> active (this is the case the old code mislabeled not-installed)
        (None, "/custom/root/bin/fno", True, "active"),
        # `fno` on PATH IS the cargo mux -> active (matched by path, probe not needed)
        ("/home/x/.cargo/bin/fno", "/home/x/.cargo/bin/fno", False, "active"),
        # cargo mux installed but a non-mux `fno` wins PATH -> shadowed
        ("/home/x/.cargo/bin/fno", "/home/x/.local/bin/fno", False, "shadowed"),
        # cargo mux installed but off PATH -> shadowed
        ("/home/x/.cargo/bin/fno", None, False, "shadowed"),
    ],
)
def test_mux_front_door_report_states(
    monkeypatch: pytest.MonkeyPatch, mux, which_fno, probe, expected
) -> None:
    """Front-door state: active when `fno` on PATH is the mux (== cargo binary OR
    answers the mux verb, catching custom --root); shadowed when a cargo mux
    exists but isn't the `fno` on PATH; not-installed otherwise."""
    monkeypatch.setattr(doctor, "_cargo_installed_mux", lambda: Path(mux) if mux else None)
    monkeypatch.setattr(doctor.shutil, "which", lambda name: which_fno)
    monkeypatch.setattr(doctor, "_probe_is_mux", lambda p: probe)
    report = doctor._mux_front_door_report()
    assert report["mux_front_door"] == expected
    assert report["mux_binary"] == (mux if mux else None)
    assert report["path_fno"] == which_fno


# ---------------------------------------------------------------------------
# Orphan-file report (Group 3 GC)
# ---------------------------------------------------------------------------


def test_orphan_report_empty_on_clean_machine(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(doctor.Path, "home", classmethod(lambda cls: tmp_path / "home"))
    (tmp_path / "project").mkdir()
    monkeypatch.chdir(tmp_path / "project")
    assert doctor._orphan_report() == []


def test_orphan_report_finds_leftover_files_in_both_dirs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    home = tmp_path / "home"
    project = tmp_path / "project"
    (home / ".fno").mkdir(parents=True)
    (project / ".fno").mkdir(parents=True)
    (home / ".fno" / "convo-signals.jsonl").write_text("")
    (home / ".fno" / "tasks.json").write_text("")
    (project / ".fno" / "convo-signals.jsonl").write_text("")

    monkeypatch.setattr(doctor.Path, "home", classmethod(lambda cls: home))
    monkeypatch.chdir(project)

    report = doctor._orphan_report()
    assert str(home / ".fno" / "convo-signals.jsonl") in report
    assert str(home / ".fno" / "tasks.json") in report
    assert str(project / ".fno" / "convo-signals.jsonl") in report
    assert len(report) == 3


def test_orphan_report_degrades_on_unresolvable_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A deleted-out-from-under-us cwd (e.g. an archived worktree) must not
    crash the whole `fno doctor` invocation - just skip that dir."""
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    (home / ".fno" / "convo-signals.jsonl").write_text("")

    monkeypatch.setattr(doctor.Path, "home", classmethod(lambda cls: home))
    monkeypatch.setattr(
        doctor.Path, "cwd", classmethod(lambda cls: (_ for _ in ()).throw(OSError("gone")))
    )

    report = doctor._orphan_report()
    assert str(home / ".fno" / "convo-signals.jsonl") in report


# ---------------------------------------------------------------------------
# Content drift: ground-truth Python freshness (catches a lying installed-rev
# marker after a cache-hit reinstall).
# ---------------------------------------------------------------------------


def test_content_drift_overrides_fresh_marker(monkeypatch: pytest.MonkeyPatch) -> None:
    """The regression: marker == source HEAD (rev check says fresh) but installed
    bytes differ -> STALE, exit nonzero, message names the file count. This is the
    month-old-install-behind-a-HEAD-marker case."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # marker agrees with HEAD - the lie
        capture_present="present",
        content_drift=3,  # but 3 .py files on disk differ
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 1
    assert "STALE" in result.stdout
    assert "3 .py file" in result.stdout


def test_content_drift_zero_stays_fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    """0 differing files is byte-identical -> never flips a fresh verdict."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
        content_drift=0,
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "up to date" in result.stdout


def test_content_indeterminate_downgrades_fresh_to_unknown(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An undeterminable content check (None) must NOT leave a marker-only fresh
    standing: the marker can lie about a cache-hit reinstall, so downgrade to
    unknown. It never flips to stale (no positive drift proven)."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",  # marker matches -> rev check alone would say fresh
        capture_present="present",
        content_drift=None,  # but the ground-truth check could not run
    )
    result = runner.invoke(app, ["doctor", "--json"])
    payload = json.loads(result.stdout)
    assert payload["status"] == "unknown"
    assert payload["content_stale"] is False
    assert payload["content_indeterminate"] is True
    assert payload["content_drift_count"] is None


def test_content_indeterminate_does_not_downgrade_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A None content check only touches a would-be fresh; a verdict already proven
    stale by another signal (a missing verb) stays stale, not unknown."""
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="missing",  # proves stale independently
        content_drift=None,
    )
    result = runner.invoke(app, ["doctor", "--json"])
    payload = json.loads(result.stdout)
    assert payload["status"] == "stale"


def test_python_content_drift_counts_differing_py_files(tmp_path: Path) -> None:
    """_python_content_drift fingerprints installed vs source/src/fno and counts
    only files whose bytes differ; identical files do not count."""
    inst = tmp_path / "installed" / "fno"
    src = tmp_path / "source"
    src_pkg = src / "src" / "fno"
    inst.mkdir(parents=True)
    src_pkg.mkdir(parents=True)
    (inst / "same.py").write_text("x = 1\n")
    (src_pkg / "same.py").write_text("x = 1\n")
    (inst / "drift.py").write_text("old = True\n")
    (src_pkg / "drift.py").write_text("old = False\n")  # differs
    (src_pkg / "added.py").write_text("new = 1\n")  # only in source

    import fno as _fno_pkg

    # Point _installed_pkg_dir at our fake installed tree.
    orig_file = _fno_pkg.__file__
    try:
        _fno_pkg.__file__ = str(inst / "__init__.py")
        assert doctor._python_content_drift(src) == 2  # drift.py + added.py
    finally:
        _fno_pkg.__file__ = orig_file


def test_python_content_drift_none_when_source_missing(tmp_path: Path) -> None:
    """No source/src/fno dir -> None (skip), not a false 0 or a crash."""
    assert doctor._python_content_drift(tmp_path / "nonexistent") is None


def test_python_content_drift_none_when_source_arg_none() -> None:
    assert doctor._python_content_drift(None) is None


# ---------------------------------------------------------------------------
# Agent health (x-1c7b): four grooming surfaces shipped and never ran, every
# time because nothing reported the silence.
# ---------------------------------------------------------------------------


def _fresh(monkeypatch: pytest.MonkeyPatch) -> None:
    _stub_signals(
        monkeypatch,
        src=Path("/src"),
        source_rev="abc123",
        marker="abc123",
        capture_present="present",
    )


def test_dead_launch_agent_is_named_with_its_exit_and_reddens_doctor(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-ERR: an installed-but-failing agent must not be a quiet line."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_launch_agent_failures",
        lambda: {"applicable": True, "dead": [{"label": "sh.fno.pr-watcher", "exit": 78}]},
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 1, "a dead agent must fail the exit code, not just print"
    assert "sh.fno.pr-watcher" in result.stdout
    assert "78" in result.stdout


def test_missing_launchctl_degrades_without_crying_wolf(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-ERR: no launchctl (Linux) must never read as a dead agent."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor, "_launch_agent_failures", lambda: {"applicable": False, "dead": []}
    )
    result = runner.invoke(app, ["doctor"])
    assert result.exit_code == 0
    assert "not applicable" in result.stdout
    assert "last exited" not in result.stdout


def test_never_run_grooming_reads_differently_from_stale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC1-UI: "never" names the install remedy and prints no hour count."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    # The remedy is platform-specific, so pin it: a Linux CI runner gets the
    # cron advice and would otherwise fail a macOS-shaped assertion.
    monkeypatch.setattr(doctor.sys, "platform", "darwin")
    result = runner.invoke(app, ["doctor"])
    assert "NEVER run" in result.stdout
    assert "--install-agent" in result.stdout
    assert "h ago" not in result.stdout
    assert result.exit_code == 0, "a fresh install has legitimately never groomed"


def test_stale_grooming_reports_the_age(monkeypatch: pytest.MonkeyPatch) -> None:
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "ran", "hours": 96.0, "stale": True, "agent_installed": True},
    )
    result = runner.invoke(app, ["doctor"])
    assert "96h ago" in result.stdout
    assert "NEVER" not in result.stdout


def test_fix_installs_the_groom_agent_when_nothing_schedules_it(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    monkeypatch.setattr(doctor.sys, "platform", "darwin")  # the install is launchd-only
    calls: list = []
    monkeypatch.setattr(
        "fno.backlog.groom.install_groom_agent",
        lambda **kw: calls.append(kw) or {"status": "installed", "detail": "ok"},
    )
    result = runner.invoke(app, ["doctor", "--fix"])
    assert len(calls) == 1
    assert "--fix groom agent: installed" in result.stderr


def test_fix_skips_the_install_when_the_agent_is_already_there(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": True},
    )

    # Pinned to darwin so this proves the already-installed guard, not the
    # platform guard - off launchd it would pass without exercising anything.
    monkeypatch.setattr(doctor.sys, "platform", "darwin")

    def _boom(**kw):
        raise AssertionError("must not reinstall an agent that is already installed")

    monkeypatch.setattr("fno.backlog.groom.install_groom_agent", _boom)
    runner.invoke(app, ["doctor", "--fix"])


def test_agent_scan_parses_last_exit_not_current_state(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A `-` in the PID column is normal for a periodic job; only col 2 counts."""
    import subprocess as sp

    monkeypatch.setattr(doctor.sys, "platform", "darwin")
    monkeypatch.setattr(doctor.shutil, "which", lambda name: "/bin/launchctl")
    listing = (
        "PID\tStatus\tLabel\n"
        "-\t0\tsh.fno.groom\n"
        "-\t78\tsh.fno.pr-watcher\n"
        "412\t0\tsh.fno.mux\n"
        "-\t127\tcom.other.thing\n"
        "-\t-\tsh.fno.idle\n"
    )
    monkeypatch.setattr(
        doctor.subprocess,
        "run",
        lambda *a, **kw: sp.CompletedProcess(a[0], 0, listing, ""),
    )
    report = doctor._launch_agent_failures()
    assert report["applicable"] is True
    assert report["dead"] == [{"label": "sh.fno.pr-watcher", "exit": 78}], (
        "only nonzero-exit sh.fno.* labels count; foreign labels and `-` do not"
    )


def test_never_run_remedy_is_platform_appropriate(monkeypatch: pytest.MonkeyPatch) -> None:
    """--install-agent is launchd-only; off darwin it would report `unsupported`."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    monkeypatch.setattr(doctor.sys, "platform", "linux")
    result = runner.invoke(app, ["doctor"])
    assert "NEVER run" in result.stdout
    assert "--install-agent" not in result.stdout, "that flag does nothing off launchd"
    assert "docs/backlog-usage.md" in result.stdout


def test_fix_does_not_attempt_a_launchd_install_off_darwin(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An unguarded call would warn `unsupported` on every --fix, unactionably."""
    _fresh(monkeypatch)
    monkeypatch.setattr(
        doctor,
        "_groom_health",
        lambda: {"state": "never", "hours": None, "stale": True, "agent_installed": False},
    )
    monkeypatch.setattr(doctor.sys, "platform", "linux")

    def _boom(**kw):
        raise AssertionError("must not attempt a launchd install off darwin")

    monkeypatch.setattr("fno.backlog.groom.install_groom_agent", _boom)
    result = runner.invoke(app, ["doctor", "--fix"])
    assert "groom agent" not in result.stderr

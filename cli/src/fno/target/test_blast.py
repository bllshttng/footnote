"""Unit tests for the blast-radius classifier + `fno target blast-check` verb.

Covers task 1.1 acceptance (AC1-EDGE empty/unparseable map -> unknown; the
classifier half of AC2-EDGE/AC1-FR is exercised here; the init-modulation
behavior of those ACs lives in the task 1.2 test). The classifier is pure and
fail-safe, so every degraded path (bad glob, missing manifest, unreadable plan)
is asserted to return a verdict rather than raise.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.config import BlastConfig
from fno.target import blast
from fno.target_cli import target_app

runner = CliRunner()

# A minimal loc-ratchet manifest fixture (the include semantics are what matter).
_MANIFEST = """\
# comment
include:
  - hooks/
  - scripts/lib/
  - cli/src/fno/loop.py
  - crates/fno-agents/src/loop*
extensions:
  - sh
  - py
exclude:
  - "**/tests/**"
"""


@pytest.fixture()
def manifest(tmp_path: Path) -> str:
    p = tmp_path / "loc-ratchet-manifest.yaml"
    p.write_text(_MANIFEST, encoding="utf-8")
    return str(p)


def _cfg(**kw) -> BlastConfig:
    return BlastConfig(**kw)


# --------------------------- verdict basics ------------------------------- #
def test_empty_paths_is_unknown(manifest):
    # AC1-EDGE: empty / no usable paths -> unknown, never high/low.
    assert blast.classify([], _cfg(), manifest_path=manifest)["verdict"] == blast.UNKNOWN


def test_whitespace_only_paths_is_unknown(manifest):
    out = blast.classify(["", "   ", "\t"], _cfg(), manifest_path=manifest)
    assert out["verdict"] == blast.UNKNOWN


def test_benign_paths_are_low(manifest):
    out = blast.classify(
        ["cli/src/fno/target/blast.py", "docs/readme.md"],
        _cfg(),
        manifest_path=manifest,
    )
    assert out["verdict"] == blast.LOW
    assert out["matched_paths"] == []


def test_control_plane_manifest_path_is_high(manifest):
    out = blast.classify(["scripts/lib/config.sh"], _cfg(), manifest_path=manifest)
    assert out["verdict"] == blast.HIGH
    assert out["matched_paths"] == ["scripts/lib/config.sh"]


def test_manifest_star_entry_matches(manifest):
    # `crates/fno-agents/src/loop*` is a path-prefix glob.
    out = blast.classify(
        ["crates/fno-agents/src/loop_run.rs"], _cfg(), manifest_path=manifest
    )
    assert out["verdict"] == blast.HIGH


def test_manifest_exact_entry_matches(manifest):
    out = blast.classify(["cli/src/fno/loop.py"], _cfg(), manifest_path=manifest)
    assert out["verdict"] == blast.HIGH
    # A sibling that only shares the prefix must NOT match the exact entry.
    out2 = blast.classify(["cli/src/fno/loop_helpers.py"], _cfg(), manifest_path=manifest)
    assert out2["verdict"] == blast.LOW


# --------------------------- general globs -------------------------------- #
@pytest.mark.parametrize(
    "path",
    [
        "db/x.sql",          # **/*.sql nested
        "x.sql",             # **/*.sql at root (the leading-**/ fix)
        "supabase/migrations/0001_init.sql",
        "src/auth/login.ts",
        "lib/oauth_helper.py",   # **/*auth*
        "infra/Dockerfile",
        "terraform/main.tf",
        "app/billing/invoice.ts",
        ".env.local",
    ],
)
def test_general_high_blast_globs(path, manifest):
    out = blast.classify([path], _cfg(), manifest_path=manifest)
    assert out["verdict"] == blast.HIGH, f"{path} should be high blast"


def test_mixed_high_and_low_wins_high(manifest):
    out = blast.classify(
        ["docs/readme.md", "db/migrations/2026.sql", "src/util.ts"],
        _cfg(),
        manifest_path=manifest,
    )
    assert out["verdict"] == blast.HIGH
    assert out["matched_paths"] == ["db/migrations/2026.sql"]


# --------------------------- config knobs --------------------------------- #
def test_reuse_loc_manifest_false_drops_manifest_globs(manifest):
    # With reuse off, a control-plane-only path is no longer high.
    out = blast.classify(
        ["scripts/lib/config.sh"],
        _cfg(reuse_loc_manifest=False),
        manifest_path=manifest,
    )
    assert out["verdict"] == blast.LOW


def test_high_blast_globs_extension_matches(manifest):
    out = blast.classify(
        ["packages/core/src/router.ts"],
        _cfg(high_blast_globs=["**/router.ts"]),
        manifest_path=manifest,
    )
    assert out["verdict"] == blast.HIGH


def test_bad_glob_is_skipped_not_raised(manifest):
    # A malformed glob must not raise; it simply never matches.
    out = blast.classify(
        ["src/util.ts"],
        _cfg(high_blast_globs=["[unterminated"]),
        manifest_path=manifest,
    )
    assert out["verdict"] == blast.LOW


def test_missing_manifest_is_fail_safe(tmp_path):
    # reuse on but the manifest path does not exist -> no manifest globs, no raise.
    out = blast.classify(
        ["scripts/lib/config.sh"],
        _cfg(),
        manifest_path=str(tmp_path / "nope.yaml"),
    )
    # general globs do not cover scripts/lib, so it degrades to low (not a crash).
    assert out["verdict"] == blast.LOW


# --------------------------- normalization -------------------------------- #
def test_normalize_dot_and_relative():
    assert blast.normalize_path("./scripts/lib/x.sh") == "scripts/lib/x.sh"
    assert blast.normalize_path("a/b/../c.py") == "a/c.py"


def test_normalize_absolute_under_repo_root(tmp_path):
    (tmp_path / "scripts" / "lib").mkdir(parents=True)
    abs_path = tmp_path / "scripts" / "lib" / "config.sh"
    abs_path.write_text("x", encoding="utf-8")
    assert blast.normalize_path(str(abs_path), repo_root=str(tmp_path)) == (
        "scripts/lib/config.sh"
    )


def test_normalize_resolves_into_manifest_glob(tmp_path, manifest):
    # A relative path that resolves into scripts/lib must classify high.
    (tmp_path / "scripts" / "lib").mkdir(parents=True)
    (tmp_path / "scripts" / "lib" / "x.sh").write_text("x", encoding="utf-8")
    out = blast.classify(
        ["./scripts/lib/x.sh"], _cfg(), repo_root=str(tmp_path), manifest_path=manifest
    )
    assert out["verdict"] == blast.HIGH


# --------------------------- ownership-map parse -------------------------- #
_PLAN = """\
# Some Plan

## File Ownership Map

| File | Action | Owner |
|---|---|---|
| `cli/src/fno/target/blast.py` | create | 1.1 |
| `scripts/lib/config.sh` | modify | 1.2 |
| fno target init writer (locate in cli/src/fno) | modify | 1.2 |
| init-modulation test | create | 1.2 |

## Next Section

| File | x |
|---|---|
| `should/not/be/parsed.py` | y |
"""


def test_parse_ownership_map_keeps_paths_drops_prose():
    paths = blast.parse_ownership_map(_PLAN)
    assert "cli/src/fno/target/blast.py" in paths
    assert "scripts/lib/config.sh" in paths
    # prose rows dropped
    assert not any("writer" in p for p in paths)
    assert "init-modulation test" not in paths
    # rows outside the section are not parsed
    assert "should/not/be/parsed.py" not in paths


def test_parse_ownership_map_absent_section_is_empty():
    assert blast.parse_ownership_map("# Plan\n\nNo ownership map here.\n") == []


def test_resolve_plan_index_file_vs_dir(tmp_path):
    f = tmp_path / "plan.md"
    f.write_text("x", encoding="utf-8")
    assert blast.resolve_plan_index(str(f)) == f
    d = tmp_path / "folderplan"
    d.mkdir()
    assert blast.resolve_plan_index(str(d)) == d / "00-INDEX.md"


# --------------------------- the verb ------------------------------------- #
def test_blast_check_verb_json_and_quiet(tmp_path):
    plan = tmp_path / "plan.md"
    plan.write_text(_PLAN, encoding="utf-8")

    res = runner.invoke(target_app, ["blast-check", str(plan)])
    assert res.exit_code == 0
    import json as _json

    payload = _json.loads(res.stdout)
    assert payload["verdict"] in (blast.HIGH, blast.LOW, blast.UNKNOWN)

    res_q = runner.invoke(target_app, ["blast-check", str(plan), "--quiet"])
    assert res_q.exit_code == 0
    assert res_q.stdout.strip() in (blast.HIGH, blast.LOW, blast.UNKNOWN)


def test_blast_check_unreadable_plan_is_unknown_exit_zero(tmp_path):
    missing = tmp_path / "nope.md"
    res = runner.invoke(target_app, ["blast-check", str(missing), "--quiet"])
    assert res.exit_code == 0
    assert res.stdout.strip() == blast.UNKNOWN


# --------------------------- config defaults ------------------------------ #
def test_blast_config_defaults():
    cfg = BlastConfig()
    assert cfg.enabled is False
    assert cfg.downgrade is True
    assert cfg.reuse_loc_manifest is True
    assert cfg.high_blast_globs == []


def test_blast_config_ignores_extra_keys():
    cfg = BlastConfig(enabled=True, unknown_future_key="x")  # type: ignore[call-arg]
    assert cfg.enabled is True


def test_malformed_blast_block_fails_safe_to_disabled():
    # A non-mapping config.target.blast must not raise out of model validation;
    # it degrades to the default disabled block (the docstring promise).
    from fno.config import SettingsModel, TargetConfig

    assert TargetConfig(blast=True).blast.enabled is False  # type: ignore[arg-type]
    model = SettingsModel.model_validate(
        {"config": {"target": {"blast": "garbage"}}}
    )
    assert model.target.blast.enabled is False

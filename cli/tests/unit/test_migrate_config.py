"""`fno setup migrate-config` — legacy settings.yaml -> flat config.toml (x-8526).

The migrate round-trip is the load-bearing safety check for the hard cut: an
install must load identical values before (yaml) and after (migrated toml).
Covers AC3-HP (convert + parity), AC3-ERR (idempotent no-op), AC3-EDGE (wrapped
and already-flat both convert), AC3-FR (crash-safe atomic write).
"""
from __future__ import annotations


import pytest

from fno.config import (
    _migrate_yaml_to_toml,
    run_config_migration,
    settings_from_files,
)

WRAPPED = (
    "schema_version: 1\n"
    "config:\n"
    "  review:\n"
    "    required_bots:\n"
    "      - chatgpt-codex-connector\n"
    "  obsidian:\n"
    "    enabled: true\n"
    "    vault: myvault\n"
    "  agents:\n"
    "    max_live: 7\n"
)


def test_ac3_hp_migrate_and_load_parity(tmp_path):
    fno = tmp_path / ".fno"
    fno.mkdir()
    yaml_path = fno / "settings.yaml"
    yaml_path.write_text(WRAPPED, encoding="utf-8")

    # Baseline: values loaded straight from the legacy yaml.
    from_yaml = settings_from_files([yaml_path])

    results = run_config_migration([yaml_path])

    toml_path = fno / "config.toml"
    assert toml_path.is_file(), "migrate must write config.toml"
    assert not yaml_path.exists(), "the legacy yaml is deleted (hard cut)"
    assert (toml_path, "migrated") in results

    # Invariant: loaded-from-yaml == loaded-from-migrated-toml.
    from_toml = settings_from_files([toml_path])
    assert from_yaml.model_dump() == from_toml.model_dump()
    # Spot-check a representative value survived the round-trip.
    assert from_toml.review.required_bots == ["chatgpt-codex-connector"]
    assert from_toml.obsidian.vault == "myvault"


def test_ac3_err_idempotent_noop_when_toml_exists(tmp_path):
    fno = tmp_path / ".fno"
    fno.mkdir()
    (fno / "settings.yaml").write_text(WRAPPED, encoding="utf-8")
    toml_path = fno / "config.toml"
    toml_path.write_text('[review]\nrequired_bots = ["existing"]\n', encoding="utf-8")

    results = run_config_migration([fno / "settings.yaml"])

    # config.toml wins and is NOT clobbered; report says already-migrated.
    assert (toml_path, "already-migrated") in results
    assert settings_from_files([toml_path]).review.required_bots == ["existing"]


def test_ac3_edge_local_override_converts_to_config_local_toml(tmp_path):
    fno = tmp_path / ".fno"
    fno.mkdir()
    (fno / "settings.yaml").write_text(WRAPPED, encoding="utf-8")
    (fno / "settings.local.yaml").write_text(
        "config:\n  project:\n    id: my-worktree\n", encoding="utf-8"
    )

    run_config_migration([fno / "settings.yaml"])

    assert (fno / "config.toml").is_file()
    assert (fno / "config.local.toml").is_file()
    assert not (fno / "settings.local.yaml").exists()


def test_ac3_fr_crash_before_rename_leaves_yaml_intact(tmp_path, monkeypatch):
    fno = tmp_path / ".fno"
    fno.mkdir()
    yaml_path = fno / "settings.yaml"
    yaml_path.write_text(WRAPPED, encoding="utf-8")
    before = yaml_path.read_text(encoding="utf-8")

    # Simulate a crash at the atomic rename (temp written, rename fails).
    import fno.config as cfg

    def _boom(*_a, **_k):
        raise OSError("disk full")

    monkeypatch.setattr(cfg.os, "replace", _boom)
    with pytest.raises(OSError):
        _migrate_yaml_to_toml(yaml_path)

    # The yaml is intact, no partial config.toml, no leftover temp file.
    assert yaml_path.read_text(encoding="utf-8") == before
    assert not (fno / "config.toml").exists()
    assert list(fno.glob(".config.toml.tmp.*")) == []

    # Re-running (unpatched) completes cleanly.
    monkeypatch.undo()
    assert _migrate_yaml_to_toml(yaml_path) is not None
    assert (fno / "config.toml").is_file()
    assert not yaml_path.exists()

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import pytest
from pydantic import ValidationError

from fno.plugins.manifest import pack_digest
from fno.plugins.registry import (
    ActivationReceipt,
    ConformanceAttribution,
    PackRegistryStore,
    conformance_for,
)
from tests.unit.plugins.test_manifest import _full_pack

NOW = datetime(2026, 8, 3, 12, tzinfo=UTC)


def _receipt(pack_id: str = "growth-studio", digest: str | None = None, paths=("plugin/growth-studio/marketing.json",)) -> ActivationReceipt:
    pack = _full_pack()
    return ActivationReceipt(
        pack_id=pack_id,
        pack_digest=digest or pack_digest(pack),
        resolved_version="0.1.0",
        written_paths=paths,
        activated_at=NOW,
    )


def test_install_records_digest_version_and_declared_effects(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    pack = _full_pack()
    record = store.install(pack, Path("plugins/growth-studio/plugin.yaml"))
    assert record.pack_digest == pack_digest(pack)
    assert record.resolved_version == "0.1.0"
    assert record.declared_effects[0].effect_class == "external.publication"
    assert store.installed_index()["growth-studio"] == "0.1.0"


def test_install_is_idempotent_for_same_pack(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    pack = _full_pack()
    store.install(pack, Path("plugins/growth-studio/plugin.yaml"))
    store.install(pack, Path("plugins/growth-studio/plugin.yaml"))
    registry = store.load()
    assert len(registry.packs) == 1


def test_record_and_retrieve_activation(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    receipt = _receipt()
    store.record_activation(receipt)
    assert store.load().receipt_for("growth-studio") == receipt


def test_owner_digest_of_path_names_the_pack_that_wrote_it(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    digest_a = "a" * 64
    digest_b = "b" * 64
    store.record_activation(_receipt(pack_id="pack-a", digest=digest_a, paths=("plugin/pack-a/role.json",)))
    store.record_activation(_receipt(pack_id="pack-b", digest=digest_b, paths=("plugin/pack-b/role.json",)))
    assert store.owner_digest_of_path("plugin/pack-a/role.json") == digest_a
    assert store.owner_digest_of_path("plugin/pack-b/role.json") == digest_b
    assert store.owner_digest_of_path("plugin/hand-written.json") is None


def test_remove_activation_returns_receipt_and_clears_it(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    store.record_activation(_receipt())
    removed = store.remove_activation("growth-studio")
    assert removed is not None
    assert removed.written_paths == ("plugin/growth-studio/marketing.json",)
    assert store.load().receipt_for("growth-studio") is None
    assert store.remove_activation("growth-studio") is None


def test_conformance_for_attributes_each_adapter_to_the_pack_digest(tmp_path):
    pack = _full_pack()
    attribution = conformance_for(pack)
    digest = pack_digest(pack)
    assert len(attribution) == 1
    assert isinstance(attribution[0], ConformanceAttribution)
    assert attribution[0].pack_digest == digest
    assert attribution[0].adapter_id == "social-publisher"
    assert attribution[0].remote_idempotency is True


def test_receipt_rejects_naive_timestamp(tmp_path):
    with pytest.raises(ValidationError):
        ActivationReceipt(
            pack_id="growth-studio",
            pack_digest="a" * 64,
            resolved_version="0.1.0",
            written_paths=("plugin/x/r.json",),
            activated_at=datetime(2026, 8, 3, 12),  # naive
        )


def test_registry_round_trips_through_disk(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    pack = _full_pack()
    store.install(pack, Path("plugins/growth-studio/plugin.yaml"))
    store.record_activation(_receipt())
    # A fresh store over the same file sees the same state.
    reopened = PackRegistryStore(tmp_path / "registry.json")
    assert reopened.installed_index()["growth-studio"] == "0.1.0"
    assert reopened.load().receipt_for("growth-studio") is not None


def test_two_packs_can_each_own_their_own_paths_concurrently(tmp_path):
    store = PackRegistryStore(tmp_path / "registry.json")
    store.record_activation(_receipt(pack_id="pack-a", digest="a" * 64, paths=("plugin/pack-a/a.json",)))
    store.record_activation(_receipt(pack_id="pack-b", digest="b" * 64, paths=("plugin/pack-b/b.json",)))
    registry = store.load()
    assert {r.pack_id for r in registry.receipts} == {"pack-a", "pack-b"}


def test_corrupt_registry_load_raises_instead_of_resetting_to_empty(tmp_path):
    from fno.plugins.registry import RegistryCorrupt

    store = PackRegistryStore(tmp_path / "registry.json")
    store.path.parent.mkdir(parents=True, exist_ok=True)
    store.path.write_text("{ not valid json", encoding="utf-8")
    with pytest.raises(RegistryCorrupt):
        store.load()
    # installed_index is strict too (a corrupt registry must not verify a pack green)
    with pytest.raises(RegistryCorrupt):
        store.installed_index()
    # read-only display load degrades gracefully
    assert store.load_or_empty().packs == ()

from __future__ import annotations

from fno.graph.migrations import migrate_legacy_p0, rollback_legacy_p0


def _rows(count: int) -> list[dict]:
    return [
        {"id": f"ab-{i:08x}", "title": f"legacy {i}", "priority": "p0"}
        for i in range(count)
    ]


def test_migration_reports_and_applies_legacy_p0_population():
    entries = _rows(3)
    dry = migrate_legacy_p0(entries, apply=False)
    assert dry["legacy_p0"] == 3
    assert dry["rebanded_to_p1"] == 3
    assert dry["remaining_unacknowledged_p0"] == 3
    assert all(row["priority"] == "p0" for row in entries)

    applied = migrate_legacy_p0(entries, apply=True)
    assert applied["legacy_p0"] == 3
    assert applied["rebanded_to_p1"] == 3
    assert applied["remaining_unacknowledged_p0"] == 0
    assert all(row["priority"] == "p1" for row in entries)
    assert all(row["priority_history"][-1]["from"] == "p0" for row in entries)
    assert all(row["priority_history"][-1]["prior_priority"] == "p0" for row in entries)

    restored = rollback_legacy_p0(entries)
    assert restored["restored_to_p0"] == 3
    assert all(row["priority"] == "p0" for row in entries)


def test_migration_is_idempotent_and_preserves_acknowledged_p0():
    entries = _rows(2)
    entries.append({"id": "ab-ack0001", "priority": "p0", "blocks_everything": True})
    migrate_legacy_p0(entries, apply=True)
    second = migrate_legacy_p0(entries, apply=True)
    assert second["already_migrated"] == 2
    assert second["rebanded_to_p1"] == 0
    assert entries[-1]["priority"] == "p0"

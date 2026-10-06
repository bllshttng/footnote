//! x-43ce: one undecodable OPTIONAL spawn_provenance block must not zero the
//! whole decode. The live outage shape: a crown succession reown forked an
//! owner-only block (no `origin`) onto a provenance-less adopted row; the
//! typed reader failed the whole file, raw_rows=28 decoded_rows=0. The repair
//! strips the block (row kept, drop named); damage beyond the block still
//! fails the read.
use super::*;

/// A minimal valid row, x4c87 style.
fn plain_row(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","cwd":"/w","harness":"claude","harness_session_id":"{name}-sess","status":"live","created_at":"2026-10-06T00:00:00Z"}}"#
    )
}

/// An adopted row carrying the owner-only spawn_provenance block the bad
/// crown reown forked (no `origin`): the exact live outage shape.
fn adopted_row_with_owner_only_provenance(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","cwd":"/w","harness":"codex","harness_session_id":"{name}-sess","status":"live","created_at":"2026-10-06T00:00:00Z","origin":"adopted","spawned_by_session":"p-sess","spawn_provenance":{{"owner":{{"kind":"session","harness":"codex","session_id":"heir-sess","cwd":"/w"}}}}}}"#
    )
}

fn three_row_registry(middle: String) -> String {
    format!(
        r#"{{"schema_version":{},"agents":[{}, {}, {}]}}"#,
        REGISTRY_SCHEMA_VERSION,
        plain_row("worker-alpha"),
        middle,
        plain_row("worker-gamma"),
    )
}

#[test]
fn an_owner_only_spawn_provenance_block_no_longer_zeroes_the_decode() {
    // The live outage: one adopted row with the forked block took the whole
    // typed read down. The repair keeps every row and drops only the block.
    let dir = tmpdir("x43ce-owner-only");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        three_row_registry(adopted_row_with_owner_only_provenance("adopted-worker")),
    )
    .unwrap();

    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert_eq!(raw, 3, "the raw count still names every row on disk");
    assert_eq!(
        reg.entries.len(),
        3,
        "the malformed optional block no longer loses the row"
    );
    let adopted = reg.find("adopted-worker").expect("adopted row survives");
    assert!(
        adopted.spawn_provenance.is_none(),
        "the undecodable block is dropped, not kept: {:?}",
        adopted.spawn_provenance
    );
    std::fs::remove_dir_all(&dir).ok();
}

// The heal's two guardrails live with their primary owners: a well-formed
// block decoding intact is owned by the v33 round-trip family in mod.rs, and
// damage beside a healable-looking block staying fatal is owned by
// x4c87_row_counts (its broken row carries a valid block for exactly that).

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

/// An adopted row whose provenance block is well formed (origin plus owner).
fn adopted_row_with_valid_provenance(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","cwd":"/w","harness":"codex","harness_session_id":"{name}-sess","status":"live","created_at":"2026-10-06T00:00:00Z","origin":"adopted","spawned_by_session":"p-sess","spawn_provenance":{{"origin":{{"kind":"session","parent":{{"harness":"claude","session_id":"p-sess","cwd":"/w"}},"invocation":null}},"owner":{{"kind":"session","harness":"codex","session_id":"heir-sess","cwd":"/w"}}}}}}"#
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

#[test]
fn a_well_formed_spawn_provenance_block_decodes_intact() {
    // Cross-leg round trip: a provenance block the two legs agree on keeps
    // decoding as before; the repair never touches a healthy row.
    let dir = tmpdir("x43ce-valid-provenance");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        three_row_registry(adopted_row_with_valid_provenance("spawned-worker")),
    )
    .unwrap();

    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert_eq!(raw, 3);
    assert_eq!(reg.entries.len(), 3);
    let row = reg.find("spawned-worker").expect("spawned row survives");
    let provenance = row
        .spawn_provenance
        .as_ref()
        .expect("a well formed block survives the load");
    let crate::spawn_contract::SpawnProvenance { origin, owner } = provenance;
    assert!(matches!(
        origin,
        crate::spawn_contract::SpawnOrigin::Session { .. }
    ));
    match owner {
        crate::spawn_contract::SpawnOwner::Session(session_ref) => {
            assert_eq!(session_ref.session_id, "heir-sess");
        }
        other => panic!("expected a session owner, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn damage_elsewhere_still_fails_the_read() {
    // The heal is narrow: a row broken outside the provenance block (here a
    // status value with the wrong type) still fails the same-schema read by
    // name, exactly as before this repair existed.
    let dir = tmpdir("x43ce-damage-elsewhere");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        r#"{"schema_version":35,"agents":[
            {"name":"worker-alpha","cwd":"/w","harness":"claude","harness_session_id":"alpha-sess","status":"live","created_at":"2026-10-06T00:00:00Z"},
            {"name":"broken","cwd":"/w","harness":"claude","harness_session_id":"broken-sess","status":17,"created_at":"2026-10-06T00:00:00Z"}
        ]}"#,
    )
    .unwrap();

    let err = load_registry(&path).expect_err("damage beyond the block must still fail");
    let msg = err.to_string();
    assert!(
        matches!(err, StateError::InvariantViolation(_)),
        "same-schema damage stays fatal by name: {msg}"
    );
    assert!(msg.contains("raw_rows=2"), "names the raw count: {msg}");
    std::fs::remove_dir_all(&dir).ok();
}

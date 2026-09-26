use super::*;

// The roster parse/join family: what `parse_roster` accepts, which workers
// it skips, and how `merge_rows` joins a registry row to its roster worker.
// Lives in its own module; this file is shrink-only under the file-budget
// gate.

// The CURRENT claude shape: a bare list, as captured from
// `claude agents --json` (claude 2.1.247, 2026-08-27). The fixture is a
// mechanically redacted copy of that capture: item count, per-item key
// set and order, value types, states, and the id==sessionId-prefix
// invariant are the real document's; names/cwds/UUID tails are redacted.
#[test]
fn parse_roster_bare_list_capture_yields_every_live_session() {
    let raw = include_str!("../../../tests/testdata/roster-bare-list.json");
    let doc: serde_json::Value = serde_json::from_str(raw).unwrap();
    let items = doc.as_array().unwrap();
    let live: Vec<&serde_json::Value> = items
        .iter()
        .filter(|v| {
            !v.get("state")
                .and_then(|s| s.as_str())
                .is_some_and(is_terminal_state)
        })
        .collect();
    assert!(
        live.len() < items.len(),
        "capture must carry terminal items for this test to prove they skip"
    );
    let workers = parse_roster(raw).unwrap();
    // Positive marker 1: the parsed count equals the capture's LIVE
    // session count (terminal-catalog sessions are not roster presence),
    // and every short_id keys off the item's own id/sessionId.
    assert_eq!(
        workers.len(),
        live.len(),
        "every LIVE captured session parses; terminal ones skip"
    );
    for (w, item) in std::iter::zip(&workers, live) {
        let sid = item.get("sessionId").and_then(|v| v.as_str()).unwrap();
        assert_eq!(w.short_id, sid.split('-').next().unwrap());
        assert_eq!(w.cwd, item.get("cwd").and_then(|v| v.as_str()).unwrap());
        // Flat `name` is the bare-list field; the fallback convention
        // must not have fired for a named capture item.
        assert_eq!(w.name, item.get("name").and_then(|v| v.as_str()).unwrap());
    }
}

#[test]
fn parse_roster_bare_list_state_and_id_semantics() {
    // Terminal states skip (roster presence means attachable); the
    // explicit `id` field is the attach key, prefix is the fallback; an
    // unknown state stays (tolerant, parse_claude_agents holds unknowns).
    let raw = r#"[
        {"id":"aaaabbbb","sessionId":"ccccdddd-1","cwd":"/w","name":"live-id-wins",
         "kind":"background","startedAt":1,"state":"working"},
        {"id":"ef56ab78","sessionId":"ef56ab78-2","cwd":"/x","name":"unknown-state",
         "kind":"background","startedAt":2,"state":"weird"},
        {"id":"11112222","sessionId":"11112222-3","cwd":"/y","name":"done-skips",
         "kind":"background","startedAt":3,"state":"done"},
        {"id":"33334444","sessionId":"33334444-4","cwd":"/z","name":"stopped-skips",
         "kind":"background","startedAt":4,"state":"stopped"}]"#;
    let workers = parse_roster(raw).unwrap();
    assert_eq!(workers.len(), 2, "done and stopped skip, unknown stays");
    assert_eq!(
        workers[0].short_id, "aaaabbbb",
        "explicit id wins over prefix"
    );
    assert_eq!(workers[0].name, "live-id-wins");
    assert_eq!(workers[1].short_id, "ef56ab78");
    assert!(!workers.iter().any(|w| w.name.contains("skips")));
}

#[test]
fn parse_roster_all_terminal_roster_is_a_recognized_empty_fleet() {
    // The fleet finished: every catalog item is terminal (the daemon
    // lingers on finished sessions). Every skip was RECOGNIZED, so this
    // is an empty live fleet, not drift - Some(empty) lets the sideline
    // clear instead of holding last-good rows as fake-live forever
    // (codex review round 2).
    let raw = r#"[
        {"id":"11112222","sessionId":"11112222-3","cwd":"/y","name":"a",
         "kind":"background","startedAt":3,"state":"done"},
        {"id":"33334444","sessionId":"33334444-4","cwd":"/z","name":"b",
         "kind":"background","startedAt":4,"state":"failed"}]"#;
    assert_eq!(parse_roster(raw), Some(Vec::new()));
    // Zero recognizable workers (no state, no sessionId) is still drift.
    assert_eq!(parse_roster(r#"[{"cwd":"/w"}]"#), None);
}

#[test]
fn parse_roster_skips_a_pre_warmed_spare_worker() {
    // A `dispatch.source == "spare"` worker is daemon inventory (a
    // pre-warmed idle session, empty seed), not live work: it parses to
    // no row instead of minting a `cc-<id>` phantom.
    let raw = r#"[
        {"id":"2aee5622","sessionId":"2aee5622-1111-2222-3333-444455556666","cwd":"/w",
         "kind":"background","startedAt":1,
         "dispatch":{"source":"spare","seed":{}}},
        {"id":"ccc00000","sessionId":"ccc00000-1111-2222-3333-444455556666","cwd":"/w",
         "name":"real-worker","kind":"background","startedAt":2,
         "dispatch":{"source":"fleet","seed":{"name":"real-worker"}}}]"#;
    let workers = parse_roster(raw).unwrap();
    assert_eq!(workers.len(), 1, "the spare skips, the fleet worker stays");
    assert_eq!(workers[0].name, "real-worker");
    assert!(
        !workers.iter().any(|w| w.name.starts_with("cc-")),
        "no short-id fallback row for the spare"
    );
}

#[test]
fn parse_roster_all_spare_roster_is_a_recognized_empty_fleet() {
    // Every item was understood, so an all-spare roster is an empty live
    // fleet (Some(empty)), never schema drift holding last-good rows.
    let raw = r#"[
        {"id":"2aee5622","sessionId":"2aee5622-1111-2222-3333-444455556666","cwd":"/w",
         "kind":"background","startedAt":1,
         "dispatch":{"source":"spare","seed":{}}},
        {"id":"0dc7acc5","sessionId":"0dc7acc5-1111-2222-3333-444455556666","cwd":"/w",
         "kind":"background","startedAt":2,
         "dispatch":{"source":"spare","seed":{}}}]"#;
    assert_eq!(parse_roster(raw), Some(Vec::new()));
}

#[test]
fn merge_joins_a_registry_row_whose_short_id_kept_the_full_uuid() {
    // A registry row whose minted short_id stored the FULL uuid joins its
    // roster worker by the 8-hex first segment, so no duplicate
    // `cc-<id>` foreign row is synthesized beside the row that is
    // already there (the mint-side fix lives in the registry store).
    let uuid = "49a80492-388e-44a3-bd91-017be26bcaa0";
    let owned = derive_rows(
        &reg(&format!(
            r#"{{"name":"warden","cwd":"/w","status":"live","provider":"claude","short_id":"{uuid}"}}"#
        )),
        NOW,
    )
    .unwrap();
    let rows = merge_rows(owned, &[worker("49a80492", "roster-warden", "/w")]);
    assert_eq!(rows.len(), 1, "the roster worker is owned, not foreign");
    assert_eq!(rows[0].name, "warden");
    assert!(!rows[0].external);
    // The same join owns an exited row: roster presence upgrades it
    // un-exited + external instead of leaving a dead twin behind.
    let exited = derive_rows(
        &reg(&format!(
            r#"{{"name":"stale","cwd":"/w","status":"exited","provider":"claude","short_id":"{uuid}"}}"#
        )),
        NOW,
    )
    .unwrap();
    let rows = merge_rows(exited, &[worker("49a80492", "roster-warden", "/w")]);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].exited, "roster presence revives the row");
    assert!(rows[0].external);
}

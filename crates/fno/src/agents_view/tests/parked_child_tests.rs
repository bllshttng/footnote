//! A parked fork id (`related_session_id`) renders as a
//! lineage child of its primary while the roster still lists it live
//! (AC3-HP); when nothing lists it live, nothing renders (AC4-EDGE); and the
//! id already owning its own row never double-renders.

use super::*;

fn parked_row(primary: &str, primary_sid: &str, parked_sid: &str) -> RegistryAgent {
    let mut r = plain_row(primary, None, false);
    r.harness_session_id = Some(primary_sid.into());
    r.related_session_id = Some(parked_sid.into());
    r
}

fn roster_of(id: &str) -> Vec<RosterWorker> {
    vec![RosterWorker {
        short_id: id.into(),
        name: "parked-worker".into(),
        cwd: "/w".into(),
        account: None,
    }]
}

#[test]
fn parked_id_listed_live_renders_as_a_lineage_child_ac3_hp() {
    let parent = "11111111-1111-4111-8111-111111111111";
    let parked = "22222222-2222-4222-8222-222222222222";
    let reg = vec![parked_row("primary", parent, parked)];
    let merged = merge_rows(reg, &roster_of(parked));
    let child = merged
        .iter()
        .find(|r| r.harness_session_id.as_deref() == Some(parked))
        .expect("the parked id renders as its own row");
    assert_eq!(
        child.spawned_by_session.as_deref(),
        Some(parent),
        "the child indents under its primary"
    );
    assert!(!child.external, "the child is drivable, not dim external");
    assert_eq!(child.liveness, Liveness::Alive);
    assert_eq!(
        child.attach_id.as_deref(),
        Some(parked),
        "the roster's id is the attach target"
    );
    // The registry's own row set is unchanged: one primary row.
    assert_eq!(
        merged
            .iter()
            .filter(|r| r.harness_session_id.as_deref() == Some(parent))
            .count(),
        1
    );
    // The lineage join indents the child beneath the primary.
    let (order, depths) = lineage_layout(
        &merged,
        |r| r.harness_session_id.as_deref(),
        |r| r.spawned_by_session.as_deref(),
    );
    let child_pos = merged
        .iter()
        .position(|r| r.harness_session_id.as_deref() == Some(parked))
        .unwrap();
    let parent_pos = merged
        .iter()
        .position(|r| r.harness_session_id.as_deref() == Some(parent))
        .unwrap();
    assert!(
        depths[order.iter().position(|&i| i == child_pos).unwrap()]
            == depths[order.iter().position(|&i| i == parent_pos).unwrap()] + 1,
        "the child sits exactly one level beneath its parent"
    );
}

#[test]
fn parked_id_not_listed_live_renders_nothing_ac4_edge() {
    let parent = "33333333-3333-4333-8333-333333333333";
    let parked = "44444444-4444-4444-8444-444444444444";
    let reg = vec![parked_row("primary", parent, parked)];
    // An EMPTY roster: nothing lists the parked id live.
    let merged = merge_rows(reg, &[]);
    assert!(
        merged
            .iter()
            .all(|r| r.harness_session_id.as_deref() != Some(parked)),
        "no child row without a live listing"
    );
}

#[test]
fn parked_id_already_owning_a_row_never_double_renders() {
    let parent = "55555555-5555-4555-8555-555555555555";
    let parked = "66666666-6666-4666-8666-666666666666";
    let mut owned = parked_row("branch", parent, parked);
    // The BRANCH arm minted the parked id its own row: the id is owned.
    owned.harness_session_id = Some(parked.into());
    owned.related_session_id = None;
    let mut primary = parked_row("primary", parent, parked);
    primary.related_session_id = Some(parked.into());
    // Note: `owned` also still matches the ownership check via its own id.
    let reg = vec![owned, primary];
    let merged = merge_rows(reg, &roster_of(parked));
    assert_eq!(
        merged
            .iter()
            .filter(|r| r.harness_session_id.as_deref() == Some(parked))
            .count(),
        1,
        "the real branch row is the only render"
    );
}

// -- task 4.1 reader half: served liveness outranks the status string -------

#[test]
fn served_liveness_beats_a_stale_status_when_fresh() {
    // A fresh `alive` measurement answers Alive even though the stored
    // status still reads `orphaned` - the exact t-x30c2-w1 lie, read
    // correctly on the render path.
    let raw = r#"{"schema_version": 6, "agents": [{"name": "served", "cwd": "/w", "status": "orphaned", "liveness": "alive", "liveness_measured_at": "MEASURED_AT"}]}"#;
    let (rows, _) = derive_rows_counted(&raw.replace("MEASURED_AT", &now_stamp(0)), NOW).unwrap();
    assert_eq!(rows[0].liveness, Liveness::Alive);
    assert_eq!(rows[0].liveness_age_s, Some(0));
}

#[test]
fn stale_served_liveness_falls_back_to_the_status_ladder() {
    // A measurement older than the freshness window is not trusted: the
    // ladder answers (orphaned -> Unmeasured here), never a stale `alive`.
    let raw = r#"{"schema_version": 6, "agents": [{"name": "served", "cwd": "/w", "status": "orphaned", "liveness": "alive", "liveness_measured_at": "MEASURED_AT"}]}"#;
    let (rows, _) =
        derive_rows_counted(&raw.replace("MEASURED_AT", &now_stamp(3600)), NOW).unwrap();
    assert_eq!(rows[0].liveness, Liveness::Unmeasured);
    // The age is still carried so the render can say "probe older than N s".
    assert_eq!(rows[0].liveness_age_s, Some(3600));
}

#[test]
fn served_dead_reads_dead() {
    let raw = r#"{"schema_version": 6, "agents": [{"name": "served", "cwd": "/w", "status": "live", "liveness": "dead", "liveness_measured_at": "MEASURED_AT"}]}"#;
    let (rows, _) = derive_rows_counted(&raw.replace("MEASURED_AT", &now_stamp(1)), NOW).unwrap();
    assert_eq!(rows[0].liveness, Liveness::Dead);
}

/// An RFC3339-like stamp `age` seconds before NOW (1_800_000_000 =
/// 2027-01-15T08:00:00Z), same fixed format the registry writes and
/// `rfc3339_like_to_secs` parses. Explicit table: the parser is strict.
fn now_stamp(age: u64) -> String {
    match age {
        0 => "2027-01-15T08:00:00Z".to_string(),
        1 => "2027-01-15T07:59:59Z".to_string(),
        3600 => "2027-01-15T07:00:00Z".to_string(),
        _ => unreachable!("only these three ages are used"),
    }
}

#[tokio::test]
async fn watch_registry_decodes_a_served_document_and_the_unchanged_answer() {
    // A fake daemon: one accept, one framed answer per
    // connection. The served case returns the version as a stamp (the same
    // (mtime, len) domain the file scan gates with) plus the document;
    // the unchanged case (doc null) returns the stamp with no document.
    let dir = std::env::temp_dir().join(format!("fno-watch-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("supervisor.sock");
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        for answer in [
            r#"{"id":1,"result":{"version":{"mtime_nanos":123,"len":4},"doc":{"schema_version":6,"agents":[{"name":"served-row","cwd":"/w"}]}}}"#,
            r#"{"id":1,"result":{"version":{"mtime_nanos":123,"len":4},"doc":null}}"#,
        ] {
            let (conn, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(conn);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            reader.get_mut().write_all(answer.as_bytes()).await.unwrap();
            reader.get_mut().write_all(b"\n").await.unwrap();
        }
    });
    // Give the accept loop a moment to start; the client has its own bound.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let (stamp, raw) = watch_registry(None, &sock_path).await.unwrap();
    let raw = raw.expect("first answer serves the document");
    assert!(raw.contains("served-row"), "{raw}");
    let stamp = stamp.expect("a served answer carries its version");
    let (t, len) = stamp;
    assert_eq!(
        t.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64,
        123
    );
    assert_eq!(len, 4);

    let (stamp2, raw2) = watch_registry(None, &sock_path).await.unwrap();
    assert!(raw2.is_none(), "a null doc is the unchanged answer");
    assert!(stamp2.is_some());

    server.await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn watch_registry_fails_closed_when_nothing_listens() {
    // A missing socket is an Err, which the scan loop reads as "fall back to
    // the file reader" (AC13) - never as an empty roster.
    let missing = std::env::temp_dir().join(format!("fno-watch-absent-{}", std::process::id()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let answer = rt.block_on(watch_registry(None, &missing));
    assert!(answer.is_err(), "absent socket must Err");
}

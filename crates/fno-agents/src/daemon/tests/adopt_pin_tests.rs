//! The adopt lane refuses, before any claim or spawn, a session it cannot
//! carry: a routed row, a non-default account, or an unpinnable model. The
//! routed-row case here is hermetic end to end (no transcript, no route dir
//! read), beside the argv-level tests in claude_stream_entry.rs.

use super::*;
use crate::daemon::tests::{short_home, test_ctx};

/// Seed an EXITED claude row for the adopt target: the one-host pre-check
/// only refuses LIVE rows, so a stopped worker's row is re-adoptable in
/// principle - and exactly the shape that must refuse on an uncarryable
/// launch (routed, non-default account, unpinnable model).
fn seed_routed_exited_row(home: &AgentsHome, name: &str, short_id: &str, uuid: &str) {
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            harness: Some("claude".into()),
            name: name.into(),
            short_id: short_id.into(),
            legacy_provider: "claude".into(),
            provider: Some("zai".into()),
            requested_model: Some("glm-5.3-flash[1m]".into()),
            model: Some("glm-5.3-flash[1m]".into()),
            claude_session_uuid: Some(uuid.into()),
            host_mode: Some(crate::state::HOST_MODE_INTERACTIVE.into()),
            status: AgentStatus::Exited,
            created_at: "2026-09-21T00:00:00Z".into(),
            cwd: "/tmp".into(),
            project_root: "/severed/tmp".into(),
            route_settings_path: Some("/tmp/x20ac-route.json".into()),
            launch_account: Some("default".into()),
            ..Default::default()
        });
    })
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn routed_row_adopt_refuses_before_any_claim() {
    let home = short_home("adoptpin");
    seed_routed_exited_row(&home, "first", "swAP", "uuid-swAP");
    let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
    let req = Request::new(
        1,
        "agent.spawn",
        json!({
            "name": "cl2", "provider": "claude", "host_mode": "interactive",
            "resume_id": "uuid-swAP"
        }),
    );
    let resp = handle_spawn(&ctx, &req).await;
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidParams);
            assert!(
                e.message.contains("fno agents resume") && e.message.contains("first"),
                "adopt refusal must name the door and the row; got: {}",
                fno_message(&resp)
            );
        }
        _ => panic!("expected adopt refusal for a routed row"),
    }
    // No claim was taken: the refusal fired before the single-writer acquire,
    // so the pinned claims root holds no session claim for the uuid.
    let claims = home.root().join("claims-root");
    let stray = std::fs::read_dir(&claims)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    assert!(
        !stray,
        "the claims root must hold nothing after a pre-claim refusal (checked {})",
        claims.display()
    );
    std::fs::remove_dir_all(home.root()).ok();
}

fn fno_message(resp: &crate::protocol::Response) -> String {
    match &resp.payload {
        crate::protocol::ResponsePayload::Err(e) => e.message.clone(),
        _ => String::new(),
    }
}

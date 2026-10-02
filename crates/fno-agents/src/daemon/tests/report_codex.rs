//! The adopted-codex repro: a codex row carrying no session id,
//! only its rollout path, must get its reports STORED via the rollout
//! backfill -- never buffered-forever as an unknown session. Plus the
//! ambiguity refusal and the isolation guard: the repro runs on
//! a fresh temp home; shared-or-unset fleet env is refused, never targeted.

use super::*;
use crate::daemon::tests::{short_home, test_ctx};

/// The rollout thread id shape a real codex rollout names.
const TID: &str = "0f0e1d2c-3b4a-4958-8675-3092f4c1b2a3";

/// Isolation guard (AC2): the repro refuses to start on unset fleet env.
/// The repro itself never consumes these vars (it builds its own temp home),
/// so the guard only has to hold: no run reaches a daemon with the shared
/// home implied.
fn require_isolated_fleet<'a>(
    mux: Option<&'a str>,
    agents_home: Option<&'a str>,
) -> Result<(&'a str, &'a str), String> {
    match (mux, agents_home) {
        (Some(m), Some(h)) if !m.is_empty() && !h.is_empty() => Ok((m, h)),
        _ => Err(
            "refusing: FNO_MUX_DIR and FNO_AGENTS_HOME must name an isolated \
             mktemp pair; running with them unset would target the shared fleet"
                .to_string(),
        ),
    }
}

fn seed_kestrel_row(home: &AgentsHome, name: &str, rollout: &std::path::Path) {
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            harness: Some("codex".into()),
            name: name.into(),
            short_id: String::new(),
            legacy_provider: String::new(),
            provider: Some("openai".into()),
            status: AgentStatus::Orphaned,
            created_at: "2026-10-01T00:00:00Z".into(),
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            log_path: Some(rollout.to_string_lossy().into_owned()),
            origin: Some("adopted".into()),
            substrate: None,
            ..Default::default()
        });
    })
    .unwrap();
}

fn rollout_under(home: &AgentsHome, tag: &str) -> std::path::PathBuf {
    let dir = home.root().join("sessions").join(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("rollout-2026-10-01T00-00-00-{TID}.jsonl"));
    std::fs::write(&p, "{}\n").unwrap();
    p
}

/// The negative join: a codex row whose log_path is a plain transcript can
/// never adopt a report id (replaces skips_non_claude_rows).
fn seed_plain_path_row(home: &AgentsHome) {
    seed_kestrel_row(home, "plain", std::path::Path::new("/tmp/w.log"));
}

#[test]
fn codex_reports_store_via_the_rollout_backfill() {
    // AC2 guard: both halves of the isolation pair are demanded.
    assert!(require_isolated_fleet(None, Some("/tmp/x")).is_err());
    assert!(require_isolated_fleet(Some("/tmp/x"), None).is_err());
    assert!(require_isolated_fleet(Some(""), Some("")).is_err());
    assert!(require_isolated_fleet(Some("/tmp/x"), Some("/tmp/y")).is_ok());

    let home = short_home("repcx");
    let rollout = rollout_under(&home, "a");
    seed_kestrel_row(&home, "kestrel", &rollout);
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    // A codex Stop payload: session_id is the rollout thread id.
    let resp = handle_report(
        &ctx,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": TID, "seq": 1, "state": "done"}),
        ),
    );
    assert!(!resp.is_err(), "report must return Ok: {resp:?}");
    assert_eq!(
        resp.result().unwrap()["stored"],
        true,
        "the report must store on the adopted codex row, not buffer forever"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let e = &reg.entries[0];
    assert_eq!(e.harness_session_id.as_deref(), Some(TID));
    assert_eq!(e.codex_session_id.as_deref(), Some(TID));
    let rep = e
        .inside_leg
        .as_ref()
        .expect("report stored on the adopted row");
    assert_eq!(rep.state, state::InsideLegState::Done);
    std::fs::remove_dir_all(home.root()).ok();

    // A codex row whose log_path is not a rollout can never adopt a report
    // id (the old matcher contract, kept under the widened fn).
    let plain = short_home("repcx3");
    seed_plain_path_row(&plain);
    let ctx2 = test_ctx(plain.clone(), PathBuf::from("fno-agents-worker"));
    let resp2 = handle_report(
        &ctx2,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": TID, "seq": 1, "state": "working"}),
        ),
    );
    assert_eq!(resp2.result().unwrap()["stored"], false);
    assert_eq!(resp2.result().unwrap()["buffered"], true);
    let reg2 = state::load_registry(&plain.registry_json()).unwrap();
    assert!(reg2.entries[0].harness_session_id.is_none());
    assert!(reg2.entries[0].inside_leg.is_none());
    std::fs::remove_dir_all(plain.root()).ok();

    // Ambiguity (AC4): two id-less codex rows naming the SAME rollout thread
    // id -- neither may be backfilled (AC1-ERR, the claude rule).
    let amb = short_home("repcx2");
    let ra = rollout_under(&amb, "a");
    let rb = rollout_under(&amb, "b");
    seed_kestrel_row(&amb, "row-a", &ra);
    seed_kestrel_row(&amb, "row-b", &rb);
    let ctx3 = test_ctx(amb.clone(), PathBuf::from("fno-agents-worker"));
    let resp3 = handle_report(
        &ctx3,
        &Request::new(
            1,
            "agent.report",
            json!({"session_id": TID, "seq": 1, "state": "working"}),
        ),
    );
    assert_eq!(resp3.result().unwrap()["stored"], false);
    assert_eq!(resp3.result().unwrap()["buffered"], true);
    let reg3 = state::load_registry(&amb.registry_json()).unwrap();
    for e in &reg3.entries {
        assert!(e.harness_session_id.is_none(), "no backfill on ambiguity");
        assert!(e.codex_session_id.is_none());
        assert!(e.inside_leg.is_none());
    }
    assert_eq!(
        ctx3.pending_inside_leg.lock().unwrap().len(),
        1,
        "the refused report stays buffered, not dropped"
    );
    std::fs::remove_dir_all(amb.root()).ok();
}

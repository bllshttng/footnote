//! Reader pins for the thread-row status question (file budget: moved out of
//! agents_view.rs's inline tests so the over-budget file shrinks).
use super::*;

fn reg(rows: &str) -> String {
    format!(r#"{{"schema_version": 6, "agents": [{rows}]}}"#)
}

fn now() -> u64 {
    rfc3339_like_to_secs("2027-01-15T08:00:00Z").unwrap()
}

#[test]
fn agent_rows_codex_thread_badges_from_its_driver_report() {
    // x-fd66: a thread row hosts no pane, and its badge still answers -
    // from the report its DRIVER wrote into `inside_leg` (claude: the
    // inside-leg hooks; codex: the daemon's turn state). A live `working`
    // report badges Working; one aged past its ttl ages to Unmeasured,
    // never to a cheerful default; and a `done` report carries no ttl, so
    // a thread at its prompt reads Done for its whole idle life. No
    // production change here: this pins that the reader rungs already
    // ordered inside-leg above scrape for thread rows too.
    let raw = reg(&format!(
        r#"{{"name":"thread-working","cwd":"/w","status":"live","harness":"codex",
             "substrate":"thread",
             "inside_leg":{{"state":"working","seq":2,
                            "received_at":"2027-01-15T07:59:30Z","ttl_ms":90000}}}},
           {{"name":"thread-lapsed","cwd":"/w","status":"live","harness":"codex",
             "substrate":"thread",
             "inside_leg":{{"state":"working","seq":3,
                            "received_at":"2020-01-01T00:00:00Z","ttl_ms":90000}}}},
           {{"name":"thread-done","cwd":"/w","status":"live","harness":"codex",
             "substrate":"thread",
             "inside_leg":{{"state":"done","seq":4,
                            "received_at":"2020-01-01T00:00:00Z"}}}}"#
    ));
    let rows = derive_rows(&raw, now()).unwrap();
    let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
    assert_eq!(
        get("thread-working").badge,
        Some(AgentBadge::Working),
        "a mid-turn driver report badges with no pane attached"
    );
    assert_eq!(
        get("thread-lapsed").badge,
        None,
        "a ttl'd report past its ttl ages to Unmeasured, not Working"
    );
    assert_eq!(
        get("thread-done").badge,
        Some(AgentBadge::Done),
        "a done report never ages: the thread is at its prompt"
    );
}

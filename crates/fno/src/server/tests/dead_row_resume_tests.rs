//! When does a registry row resume: the dead-row disposition family.
use super::*;

#[test]
fn row_resume_disposition_unmeasured_names_the_absent_reading() {
    // (x-d401, AC2-EDGE) Liveness::Unmeasured is NOT a dead backend, and
    // must not print one. The old fold returned the dead-backend refusal
    // for every non-Alive reading, so a row whose pane was live eight rows
    // down the same sideline told the operator its backend was not live.
    // An unmeasured reading names the absent reading; a positive dead
    // reading resumes.
    let base = || RegistryAgent {
        harness_session_id: Some("01a027ad".into()),
        harness: Some("codex".into()),
        name: "w".into(),
        cwd: "/w".into(),
        exited: true,
        liveness: agents_view::Liveness::Alive,
        ..Default::default()
    };
    let mut unmeasured = base();
    unmeasured.exited = false;
    unmeasured.liveness = agents_view::Liveness::Unmeasured;
    assert_eq!(
        Core::row_resume_disposition(&unmeasured),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivenessUnmeasured),
        "an unmeasured backend must read as no-reading, never as dead"
    );
    let mut dead = base();
    dead.exited = false;
    dead.liveness = agents_view::Liveness::Dead;
    assert_eq!(
        Core::row_resume_disposition(&dead),
        RowResumeDisposition::Resumable,
        "a positive dead reading resumes whatever the status word says"
    );
}

#[test]
fn orphaned_row_with_a_fresh_dead_reading_resumes() {
    let raw = r#"{"agents": [{"name": "king-4d9b-delivery", "cwd": "/w",
        "status": "orphaned", "harness": "codex",
        "harness_session_id": "01a09bcd-8b5f-7391-83f8-d9ed91b00ac5",
        "liveness": "dead", "liveness_measured_at": "2026-09-15T20:14:55Z",
        "mux": {"session": "main", "pane_id": 2277}}]}"#;
    let (rows, _) = agents_view::derive_rows_counted(raw, 1_789_503_325).expect("parses");
    assert!(!rows[0].exited, "the status word alone does not say dead");
    assert_eq!(rows[0].liveness, agents_view::Liveness::Dead);
    assert_eq!(
        Core::row_resume_disposition(&rows[0]),
        RowResumeDisposition::Resumable
    );
    let live = raw.replace(r#""liveness": "dead""#, r#""liveness": "alive""#);
    let (rows, _) = agents_view::derive_rows_counted(&live, 1_789_503_325).expect("parses");
    assert_eq!(
        Core::row_resume_disposition(&rows[0]),
        RowResumeDisposition::NoPane(AgentNoPaneReason::LivePaneless)
    );
}

//! Row-set join tests, moved out of `server_tests.rs` with the
//! driving-session join they assert: the file is over budget and may only
//! shrink, and the join is row_set's question.

use super::{bg_row, empty_core};
use std::collections::HashMap;

#[test]
fn agent_rows_join_pr_from_holder_map() {
    // A name-resolved row gets its pr without a claim; a holder-only row
    // keeps the harness-native fallback. updated_at passes through.
    let mut core = empty_core();
    core.session_name = "main".into();
    let mut worker = bg_row("t-xdae5-reviewflags-glm", "/w", None);
    worker.updated_at = Some(42);
    core.agents = vec![worker, bg_row("holder-only", "/x", None)];
    core.backlog_holders = HashMap::from([("x-9c5f".to_string(), "holder-only".to_string())]);
    core.backlog_pr = HashMap::from([("x-dae5".to_string(), 999), ("x-9c5f".to_string(), 385)]);
    core.backlog_driver = HashMap::from([("x-dae5".to_string(), "09234474".to_string())]);
    let rows = core.agent_rows();
    let joined = rows
        .iter()
        .find(|r| r.name == "t-xdae5-reviewflags-glm")
        .unwrap();
    assert_eq!(joined.pr, Some(999));
    assert_eq!(joined.updated_at, Some(42));
    // The attach handle joins through the same node resolution as the pr:
    // the row names the driving session even when its own session id differs.
    assert_eq!(joined.pr_session_short.as_deref(), Some("09234474"));
    let fallback = rows.iter().find(|r| r.name == "holder-only").unwrap();
    assert_eq!(fallback.pr, Some(385));
    // No driver map entry behind the fallback's pr: the row says so.
    assert_eq!(fallback.pr_session_short, None);
}

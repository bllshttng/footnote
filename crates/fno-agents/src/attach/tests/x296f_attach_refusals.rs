//! The attach-refusal family (x-296f), beside the attach verb it exercises.
//! Parent helpers resolve through the glob.
use serde_json::json;

use super::*;

/// AC11-ERR (x-296f): a codex thread row with NO session id on file
/// refuses by naming the missing rollout and pointing at `peek --follow`.
/// The vendor's bare "no rollout found for thread id" never reaches the
/// operator through this door. It never returns `None` (the "not a
/// thread, use the refusal" answer), because that would print a message
/// saying codex has no persistent session when the row IS a thread.
#[test]
fn a_codex_thread_row_with_no_session_id_refuses_naming_the_rollout() {
    let home = std::env::temp_dir().join(format!("fno-x296f-nosess-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let events = home.join("events.jsonl");
    let entry = json!({
        "name": "cx", "harness": "codex", "cwd": "/w",
        "host_mode": "interactive", "short_id": "",
    });

    let outcome = attach_via_declared_form("codex", &entry, "cx", &events);

    assert_eq!(outcome, Some(13));
    let log = std::fs::read_to_string(&events).unwrap_or_default();
    assert!(
        log.contains("no-session-id-yet"),
        "the refusal must name its own reason in the event log: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// AC11-ERR (x-296f): the same attach with stdin not a terminal refuses
/// by naming the terminal requirement, with its own event reason - not
/// the vendor's bare "stdin is not a terminal" with no clue which command
/// produced it.
#[test]
fn a_codex_thread_attach_without_a_tty_refuses_naming_the_terminal() {
    // cargo test runs with stdin detached, so the isatty probe is false
    // here by construction; the exec path is unreachable in this suite.
    let home = std::env::temp_dir().join(format!("fno-x296f-notty-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let events = home.join("events.jsonl");
    let entry = json!({
        "name": "cx", "harness": "codex", "cwd": "/w",
        "host_mode": "interactive", "short_id": "",
        "harness_session_id": "01a04546-28b2-7a41-ae4c-892bbeb8e295",
    });

    let outcome = attach_via_declared_form("codex", &entry, "cx", &events);

    assert_eq!(outcome, Some(13));
    let log = std::fs::read_to_string(&events).unwrap_or_default();
    assert!(
        log.contains("\"no-tty\""),
        "the refusal must name its own reason in the event log: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// AC12 / AC4 (x-296f): a codex row that is NOT thread-shaped answers
/// `None` - the caller keeps its verbatim refusal, exit code included -
/// and a harness that declares no form does the same whatever its shape.
#[test]
fn non_thread_rows_and_undeclaring_harnesses_fall_through_to_the_refusal() {
    let home = std::env::temp_dir().join(format!("fno-x296f-fall-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let events = home.join("events.jsonl");
    let thread = json!({
        "name": "cx", "harness": "codex", "cwd": "/w",
        "host_mode": "interactive", "short_id": "",
        "harness_session_id": "01a04546-28b2-7a41-ae4c-892bbeb8e295",
    });

    // A pane row: its process already has a place.
    let mut pane = thread.clone();
    pane["mux"] = json!({"session": "s", "pane_id": 4});
    assert_eq!(
        attach_via_declared_form("codex", &pane, "cx", &events),
        None
    );
    // A one-shot ask row: not a thread either.
    let ask = json!({
        "name": "cx-ask", "harness": "codex", "cwd": "/w",
        "short_id": "",
        "harness_session_id": "01a04546-28b2-7a41-ae4c-892bbeb8e295",
    });
    assert_eq!(attach_via_declared_form("codex", &ask, "cx", &events), None);
    // gemini declares nothing: even a thread-shaped row falls through.
    let gm = json!({
        "name": "gm", "harness": "gemini", "cwd": "/w",
        "host_mode": "interactive", "short_id": "",
        "harness_session_id": "01a04546-28b2-7a41-ae4c-892bbeb8e295",
    });
    assert_eq!(attach_via_declared_form("gemini", &gm, "gm", &events), None);
    assert_eq!(
        std::fs::read_to_string(&events).unwrap_or_default(),
        "",
        "a fall-through owns no outcome and emits no event"
    );
    std::fs::remove_dir_all(&home).ok();
}

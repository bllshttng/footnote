use super::super::intent::detect_intent_full;
use super::*;

#[test]
fn detect_intent_promise() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "done <promise>COMPLETE</promise>"}
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    assert_eq!(detect_intent_full(&path), Intent::Promise);
}

#[test]
fn detect_intent_aborted_beats_promise() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    // Last line has aborted (even if earlier had promise, aborted in same msg wins)
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "<aborted reason=\"user\">done</aborted>"}
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    assert!(matches!(detect_intent_full(&path), Intent::Aborted { .. }));
}

#[test]
fn detect_intent_tool_result_ignored() {
    // Tool result content with promise-like text should not trigger
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let user_line = serde_json::json!({
        "message": {"role": "user", "content": "<promise>fake</promise>"}
    });
    std::fs::write(&path, serde_json::to_string(&user_line).unwrap() + "\n").unwrap();
    assert_eq!(detect_intent_full(&path), Intent::None);
}

#[test]
fn detect_intent_none_when_no_assistant() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({"message": {"role": "user", "content": "go"}});
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    assert_eq!(detect_intent_full(&path), Intent::None);
}

#[test]
fn detect_intent_array_content_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "<promise>done</promise>"},
                {"type": "tool_use", "name": "Bash"}
            ]
        }
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    assert_eq!(detect_intent_full(&path), Intent::Promise);
}

#[test]
fn extract_last_assistant_message_plain_string() {
    let payload = r#"{"transcript_path":"/t.jsonl","last_assistant_message":"  done <promise>MISSION COMPLETE: x</promise>  "}"#;
    assert_eq!(
        extract_last_assistant_message(payload).as_deref(),
        Some("done <promise>MISSION COMPLETE: x</promise>")
    );
}

#[test]
fn extract_last_assistant_message_degrades_to_none() {
    // Missing field, malformed JSON, non-string value, and empty/blank
    // strings all degrade to None (transcript fallback), never an error.
    assert_eq!(
        extract_last_assistant_message(r#"{"transcript_path":"/t.jsonl"}"#),
        None
    );
    assert_eq!(extract_last_assistant_message("not json {"), None);
    assert_eq!(
        extract_last_assistant_message(r#"{"last_assistant_message":{"text":"obj"}}"#),
        None
    );
    assert_eq!(
        extract_last_assistant_message(r#"{"last_assistant_message":"   "}"#),
        None
    );
}

#[test]
fn detect_intent_payload_promise_wins_over_stale_transcript() {
    // AC2-HP: at the promise turn's own fire the transcript does NOT yet
    // contain the final message; the payload alone must carry the intent.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "still working on it"}
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    let (intent, source) = detect_intent(Some("<promise>MISSION COMPLETE: done</promise>"), &path);
    assert_eq!(intent, Intent::Promise);
    assert_eq!(source, "payload");
}

#[test]
fn detect_intent_payload_no_tag_is_authoritative() {
    // A tag-less payload is the stopping turn's final text; it must NOT
    // fall through to the transcript (stale-promise containment).
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "<promise>old stale promise</promise>"}
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    let (intent, source) = detect_intent(Some("moving on to other work"), &path);
    assert_eq!(intent, Intent::None);
    assert_eq!(source, "payload");
}

#[test]
fn detect_intent_payload_aborted_beats_promise() {
    let (intent, source) = detect_intent(
        Some("<promise>done</promise> <aborted reason=\"kill\">stop</aborted>"),
        Path::new("/nonexistent"),
    );
    assert!(matches!(intent, Intent::Aborted { ref reason } if reason == "kill"));
    assert_eq!(source, "payload");
}

#[test]
fn watching_intent_parses_all_attrs() {
    let (intent, source) = detect_intent(
        Some("waiting <watching reason=\"ci\" pr=\"404\" timeout=\"30m\">"),
        Path::new("/nonexistent"),
    );
    assert_eq!(source, "payload");
    assert_eq!(
        intent,
        Intent::Watching {
            reason: "ci".into(),
            pr: Some("404".into()),
            timeout: Some("30m".into()),
        }
    );
}

#[test]
fn watching_intent_malformed_attrs_default_to_absent() {
    // A bare tag: attributes absent, not an error; lease math applies its
    // own default window downstream.
    let (intent, _) = detect_intent(Some("<watching>"), Path::new("/nonexistent"));
    assert_eq!(
        intent,
        Intent::Watching {
            reason: String::new(),
            pr: None,
            timeout: None,
        }
    );
}

#[test]
fn watching_intent_aborted_beats_watching() {
    let (intent, _) = detect_intent(
        Some("<watching reason=\"ci\" pr=\"1\"> <aborted reason=\"kill\">"),
        Path::new("/nonexistent"),
    );
    assert!(matches!(intent, Intent::Aborted { ref reason } if reason == "kill"));
}

#[test]
fn watching_intent_beats_promise() {
    let (intent, _) = detect_intent(
        Some("<promise>done</promise> <watching reason=\"review\" pr=\"9\">"),
        Path::new("/nonexistent"),
    );
    assert!(matches!(intent, Intent::Watching { .. }));
}

#[test]
fn watching_intent_newest_transcript_entry_honored() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "<watching reason=\"ci\" pr=\"7\">"}
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    assert!(matches!(detect_intent_full(&path), Intent::Watching { .. }));
}

#[test]
fn watching_intent_stale_transcript_not_honored() {
    // AC3-EDGE: a watching tag 2 entries back with a tag-less newest entry
    // must NOT resurrect as Watching (payload-or-newest-entry rule).
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let mut content = String::new();
    for text in [
        "<watching reason=\"ci\" pr=\"3\">", // oldest
        "still going",
        "moving on to unrelated work", // newest
    ] {
        let line = serde_json::json!({"message": {"role": "assistant", "content": text}});
        content.push_str(&serde_json::to_string(&line).unwrap());
        content.push('\n');
    }
    std::fs::write(&path, content).unwrap();
    assert_eq!(detect_intent_full(&path), Intent::None);
}

#[test]
fn watching_intent_stale_watch_does_not_shadow_deeper_promise() {
    // A stale watching in the newest-but-one entry is skipped, and a real
    // promise deeper in the lookback window still wins.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let mut content = String::new();
    for text in [
        "<promise>MISSION COMPLETE: shipped</promise>", // oldest, real
        "<watching reason=\"ci\" pr=\"3\">",            // stale (not newest)
        "tag-less newest",                              // newest
    ] {
        let line = serde_json::json!({"message": {"role": "assistant", "content": text}});
        content.push_str(&serde_json::to_string(&line).unwrap());
        content.push('\n');
    }
    std::fs::write(&path, content).unwrap();
    assert_eq!(detect_intent_full(&path), Intent::Promise);
}

#[test]
fn detect_intent_absent_payload_falls_back_to_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "<promise>COMPLETE</promise>"}
    });
    std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();
    let (intent, source) = detect_intent(None, &path);
    assert_eq!(intent, Intent::Promise);
    assert_eq!(source, "transcript");
}

#[test]
fn detect_intent_lookback_finds_promise_behind_block_feedback() {
    // AC2-EDGE ("the block destroys the evidence"): promise 3 assistant
    // text entries back - block feedback reply + a follow-up on top -
    // must still be detected by the bounded fallback scan.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let mut content = String::new();
    for text in [
        "<promise>MISSION COMPLETE: shipped</promise>",
        "acknowledged the block; checking CI",
        "CI is still pending, waiting",
    ] {
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": text}
        });
        content.push_str(&serde_json::to_string(&line).unwrap());
        content.push('\n');
    }
    std::fs::write(&path, content).unwrap();
    assert_eq!(detect_intent_full(&path), Intent::Promise);
}

#[test]
fn detect_intent_lookback_bound_holds() {
    // AC2-EDGE ("grill the stale-promise edge"): a promise older than
    // INTENT_LOOKBACK_ENTRIES assistant text entries must NOT ride the
    // window.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("t.jsonl");
    let mut content = String::new();
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "<promise>stale</promise>"}
    });
    content.push_str(&serde_json::to_string(&line).unwrap());
    content.push('\n');
    for i in 0..INTENT_LOOKBACK_ENTRIES {
        let line = serde_json::json!({
            "message": {"role": "assistant", "content": format!("pivoted work step {i}")}
        });
        content.push_str(&serde_json::to_string(&line).unwrap());
        content.push('\n');
    }
    std::fs::write(&path, content).unwrap();
    assert_eq!(detect_intent_full(&path), Intent::None);
}

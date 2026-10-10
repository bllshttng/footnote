//! The correction meter: the lead's own retractions against operator
//! corrections, per compaction window, read from its own claude transcript.
//! Same single-read posture as [`crate::wake_meter`] and
//! [`crate::refusal_rate`]: display-only evidence for the check-in, never a
//! hand-off trigger. The reading reports counts, never a verdict: no ratio
//! field and no over flag.
//!
//! Attribution rules (measured 2026-09-26 over seven lead transcripts):
//! a retraction right after a corrective or later-acknowledged operator
//! prompt credits the operator; an acknowledged retraction after any other
//! prompt credits the peer; every other retraction is self-caught. Typed
//! versus machine prompts read through [`crate::provenance::classify_turn`],
//! the same classifier the other transcript readings use. Interrupts and
//! rejects are structural counts, never marker words.

use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

fn retraction_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)(",
            r"\bI was (wrong|mistaken|incorrect)\b",
            r"|\bI (got|had) (it|that|this) wrong\b",
            r"|\bI mis(read|stated|reported|counted|spoke|attributed|diagnosed)\b",
            r"|\bmy (earlier|previous|last|prior) (claim|statement|reading|report|answer|diagnosis|conclusion)\b[^.]{0,80}\b(wrong|false|incorrect|mistaken|overstated)\b",
            r"|\b(correction|retraction)\s*:",
            r"|\bI (retract|withdraw)\b|\bretracting\b|\bwithdrawn\b",
            r"|\bthat (claim|reading|conclusion|diagnosis) was (wrong|false|incorrect)\b",
            r"|\bI stand corrected\b|\bmy (mistake|error)\b|\bscratch that\b)",
        ))
        .expect("retraction pattern is valid")
    })
}

fn ack_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)(",
            r"\byou('re| are) right\b",
            r"|\b(L\d|lead|peer|worker|reviewer|operator|user|[0-9a-f]{8}) (is|was) right\b",
            r"|\bgood catch\b|\bfair (point|catch)\b|\bas you (said|pointed out)\b|\byour (catch|correction)\b)",
        ))
        .expect("ack pattern is valid")
    })
}

fn op_correction_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)(",
            r"\bthat'?s (not (right|true|correct|what)|wrong|false|incorrect)\b",
            r"|\b(you'?re|you are|it'?s|this is) (wrong|not right|incorrect|mistaken)\b",
            r"|\bwhy (are|did|would) you\b",
            r"|\bnot true\b|\bthat is false\b|\bincorrect\b",
            r"|\byou (said|claimed|told me)\b",
            r"|\bno,? (that|it|you|this)\b)",
        ))
        .expect("operator correction pattern is valid")
    })
}

fn wrong_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\bwrong\b").expect("wrong pattern is valid"))
}

/// The bare "wrong" alternative, with the scan's guards, case-sensitive as
/// the scan's lookbehind was: no "what's wrong", "went wrong",
/// "something/anything wrong", "go/goes/gone wrong", and no "wrong with".
/// The regex crate has no lookaround, so the context reads around each
/// match instead.
fn wrong_out_of_place(text: &str) -> bool {
    const BEFORE: [&str; 8] = [
        "what's ",
        "whats ",
        "went ",
        "something ",
        "anything ",
        "go ",
        "goes ",
        "gone ",
    ];
    wrong_re().find_iter(text).any(|m| {
        let before = &text[..m.start()];
        let after = &text[m.end()..];
        !BEFORE.iter().any(|p| before.ends_with(p)) && !after.starts_with(" with")
    })
}

fn op_corrective(text: &str) -> bool {
    op_correction_re().is_match(text) || wrong_out_of_place(text)
}

fn cleaned_text(obj: &Value) -> String {
    crate::provenance::system_reminder_re()
        .replace_all(crate::provenance::turn_text(obj).trim(), "")
        .trim()
        .to_string()
}

#[derive(Default)]
struct Window {
    self_caught: u64,
    operator_caught: u64,
    peer_caught: u64,
    interrupts: u64,
    rejects: u64,
}

impl Window {
    fn row(&self, idx: usize) -> Value {
        json!({
            "window": idx,
            "self_caught": self.self_caught,
            "operator_caught": self.operator_caught,
            "peer_caught": self.peer_caught,
            "interrupts": self.interrupts,
            "rejects": self.rejects,
        })
    }
}

/// The per-window correction counts over one claude transcript's raw text.
/// A window starts at every `compact_boundary` row; a transcript with no
/// boundary is one window. The newest window is still open, so its counts
/// are partial; readers compare the previous, closed one.
pub(crate) fn correction_meter_text(raw: &str) -> Result<Value, String> {
    let mut windows: Vec<Window> = Vec::new();
    let mut seen_rows: BTreeSet<String> = BTreeSet::new();
    let mut retracted_msgs: BTreeSet<String> = BTreeSet::new();
    // The prompt state the next assistant retraction attributes against:
    // `Some((corrective, credited))` is an operator prompt, `None` any
    // machine, relay or interrupted one. `credited` says the operator turn
    // already counted toward operator_caught on its own marker.
    let mut prompt: Option<(bool, bool)> = None;
    for (line_no, line) in raw.lines().enumerate() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if windows.is_empty() {
            windows.push(Window::default());
        }
        if row.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            windows.push(Window::default());
            continue;
        }
        if let Some(uuid) = row.get("uuid").and_then(Value::as_str) {
            if !seen_rows.insert(uuid.to_string()) {
                continue;
            }
        }
        if row.get("type").and_then(Value::as_str) == Some("assistant") {
            count_retraction(
                &row,
                line_no,
                line,
                &mut retracted_msgs,
                &prompt,
                windows.last_mut().expect("window exists"),
            );
            continue;
        }
        // A user-shaped row with text is a turn; every other row only counts
        // its tool_result rejects, the scan's no-text branch.
        if crate::provenance::is_user_turn(&row) {
            let text = cleaned_text(&row);
            if text.is_empty() {
                count_rejects(&row, windows.last_mut().expect("window exists"));
                continue;
            }
            match crate::provenance::classify_turn(&row, &crate::provenance::BusIndex::empty(), "")
            {
                crate::provenance::Provenance::Harness(
                    crate::provenance::HarnessKind::InterruptMarker,
                ) => {
                    windows.last_mut().expect("window exists").interrupts += 1;
                    prompt = None;
                }
                crate::provenance::Provenance::Operator
                | crate::provenance::Provenance::Unknown => {
                    let corrective = op_corrective(&text);
                    if corrective {
                        windows.last_mut().expect("window exists").operator_caught += 1;
                    }
                    prompt = Some((corrective, corrective));
                }
                _ => prompt = None,
            }
        } else {
            count_rejects(&row, windows.last_mut().expect("window exists"));
            // A meta row with text (stop-hook feedback, a loop wakeup) is
            // the prompt the model answered last: it ends an operator
            // prompt's reach, exactly as a typed or relayed turn does.
            if crate::provenance::is_meta_row(&row) && !cleaned_text(&row).is_empty() {
                prompt = None;
            }
        }
    }
    let total = windows.len();
    let recent: Vec<Value> = windows
        .iter()
        .enumerate()
        .rev()
        .take(2)
        .rev()
        .map(|(i, w)| w.row(i + 1))
        .collect();
    Ok(json!({"windows_total": total, "recent": recent}))
}

fn count_retraction(
    row: &Value,
    line_no: usize,
    line: &str,
    retracted: &mut BTreeSet<String>,
    prompt: &Option<(bool, bool)>,
    window: &mut Window,
) {
    let msg = row.get("message");
    let key = match msg.and_then(|m| m.get("id")).and_then(Value::as_str) {
        Some(id) => id.to_string(),
        // A row with no message id dedups on the line itself.
        None => {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            line_no.hash(&mut h);
            line.hash(&mut h);
            format!("line-{:016x}", h.finish())
        }
    };
    let Some(blocks) = msg.and_then(|m| m.get("content")).and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if retracted.contains(&key) {
            break;
        }
        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
        let Some(_m) = retraction_re().find(text) else {
            continue;
        };
        retracted.insert(key.clone());
        let ack = ack_re().is_match(text);
        match *prompt {
            Some((corrective, credited)) if corrective || ack => {
                if !credited {
                    window.operator_caught += 1;
                }
            }
            _ if ack => window.peer_caught += 1,
            _ => window.self_caught += 1,
        }
        break;
    }
}

fn count_rejects(row: &Value, window: &mut Window) {
    let content = row
        .get("message")
        .and_then(|m| m.get("content"))
        .unwrap_or(row.get("content").unwrap_or(&Value::Null));
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result")
            || !crate::provenance::json_truthy(block.get("is_error"))
        {
            continue;
        }
        let body = match block.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => continue,
        };
        if crate::refusal_rate::buckets_of(&body).any(|b| b == "user_reject") {
            window.rejects += 1;
        }
    }
}

/// Copy the flat newest-window keys the beat data carries beside the
/// window array. Not in `NUMERIC_DIFF_KEYS`: a new diff key would make the
/// first beat after deploy read unmeasured.
pub(crate) fn add_data(value: &Value, data: &mut Map<String, Value>) {
    for key in [
        "self_caught_window",
        "operator_caught_window",
        "correction_windows",
    ] {
        data.insert(key.into(), value.get(key).cloned().unwrap_or(Value::Null));
    }
}

/// The check-in's own-transcript reader: claude only. A codex rollout and
/// the opencode render carry no compaction windows a correction count can
/// read, so they report unmeasured, never a silent zero; a transcript that
/// cannot be resolved errors the reading.
pub(crate) fn r_correction_meter() -> Result<Value, String> {
    match crate::lead_checkin::own_transcript() {
        Ok(crate::lead_checkin::OwnTranscript::Text {
            harness: "claude",
            text,
            ..
        }) => {
            let mut value = correction_meter_text(&text)?;
            let flat: Option<(Value, Value)> = value
                .get("recent")
                .and_then(Value::as_array)
                .and_then(|a| a.last())
                .map(|last| (last["self_caught"].clone(), last["operator_caught"].clone()));
            if let Some((self_caught, operator_caught)) = flat {
                value["self_caught_window"] = self_caught;
                value["operator_caught_window"] = operator_caught;
            }
            // The beat data copies this key by name, so the reading carries
            // the window array under it beside `recent`.
            value["correction_windows"] = value["recent"].clone();
            Ok(value)
        }
        Ok(crate::lead_checkin::OwnTranscript::Text { harness, .. }) => {
            Ok(crate::lead_checkin::unmeasured_value(&format!(
                "no compaction windows to count corrections in for harness {harness}"
            )))
        }
        Ok(crate::lead_checkin::OwnTranscript::Unmeasured { reason, .. }) => {
            Ok(crate::lead_checkin::unmeasured_value(&reason))
        }
        Err(e) => Err(e),
    }
}

/// The `self_correction:` check-in line. The newest window is marked
/// `(open)`; the previous, closed window follows when one exists.
pub(crate) fn render_line(value: &Value) -> String {
    if let Some(reason) = value.get("unmeasured").and_then(Value::as_str) {
        return format!("self_correction: unmeasured ({reason})");
    }
    let Some(recent) = value
        .get("recent")
        .and_then(Value::as_array)
        .filter(|r| !r.is_empty())
    else {
        return "self_correction: unmeasured (no compaction window read)".to_string();
    };
    let count = |row: &Value, key: &str| row.get(key).and_then(Value::as_u64).unwrap_or(0);
    let newest = &recent[recent.len() - 1];
    let mut line = format!(
        "self_correction: window {} (open) self {} / operator {} (interrupts {}, rejects {})",
        count(newest, "window"),
        count(newest, "self_caught"),
        count(newest, "operator_caught"),
        count(newest, "interrupts"),
        count(newest, "rejects"),
    );
    if let Some(prev) = recent.len().checked_sub(2).and_then(|i| recent.get(i)) {
        line.push_str(&format!(
            "; window {} self {} / operator {}",
            count(prev, "window"),
            count(prev, "self_caught"),
            count(prev, "operator_caught"),
        ));
    }
    line
}
#[cfg(test)]
mod tests {
    use super::*;

    fn meter(raw: &str) -> Value {
        correction_meter_text(raw).expect("meter reads")
    }

    fn counts(value: &Value, idx: usize) -> (u64, u64, u64, u64, u64) {
        let row = &value["recent"][idx];
        (
            row["self_caught"].as_u64().unwrap(),
            row["operator_caught"].as_u64().unwrap(),
            row["peer_caught"].as_u64().unwrap(),
            row["interrupts"].as_u64().unwrap(),
            row["rejects"].as_u64().unwrap(),
        )
    }

    // One transcript through the whole meter. Window 1: an unprompted
    // retraction reads self; a typed correction plus its acknowledged
    // retraction reads operator once. Window 2: the structural interrupt
    // and reject counts, a corrective bare-wrong turn whose retraction
    // credits the operator without a second count, a meta row ending the
    // prompt so the next unacknowledged retraction reads self, a peer
    // acknowledgement after mail, uuid and message-id dedup, and the
    // "went wrong" guard. Window 3 exists so the reading carries the last
    // two windows only, newest last.
    #[test]
    fn window_counts() {
        let raw = concat!(
            r#"{"uuid":"w1","type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"I was wrong about the port."}]}}"#,
            "\n",
            r#"{"uuid":"w2","type":"user","message":{"content":"that's not true"}}"#,
            "\n",
            r#"{"uuid":"w3","type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"You're right, I was wrong."}]}}"#,
            "\n",
            r#"{"subtype":"compact_boundary","timestamp":"2026-10-10T00:00:00Z"}"#,
            "\n",
            r#"{"uuid":"e1","type":"user","message":{"content":"[Request interrupted by user for tool use]"}}"#,
            "\n",
            r#"{"uuid":"e2","type":"user","isMeta":true,"message":{"content":[{"type":"tool_result","is_error":true,"content":"The user doesn't want to proceed with this command"}]}}"#,
            "\n",
            r#"{"uuid":"g1","type":"user","message":{"content":"that port number was wrong"}}"#,
            "\n",
            r#"{"uuid":"g2","type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"my mistake, the port is 8080"}]}}"#,
            "\n",
            r#"{"uuid":"g3","type":"user","isMeta":true,"message":{"content":"Stop hook feedback: beat blocked"}}"#,
            "\n",
            r#"{"uuid":"g4","type":"assistant","message":{"id":"m4","content":[{"type":"text","text":"I was wrong to stage that"}]}}"#,
            "\n",
            r#"{"uuid":"p1","type":"user","message":{"content":"<fno_mail from=\"w1\">recheck the gate</fno_mail>"}}"#,
            "\n",
            r#"{"uuid":"p2","type":"assistant","message":{"id":"m5","content":[{"type":"text","text":"L1 is right and I was wrong."}]}}"#,
            "\n",
            r#"{"uuid":"d1","type":"assistant","message":{"id":"m9","content":[{"type":"text","text":"scratch that"}]}}"#,
            "\n",
            r#"{"uuid":"d1","type":"assistant","message":{"id":"m10","content":[{"type":"text","text":"scratch that"}]}}"#,
            "\n",
            r#"{"uuid":"q1","type":"user","message":{"content":"what went wrong with the build?"}}"#,
            "\n",
            r#"{"uuid":"q2","type":"assistant","message":{"id":"m11","content":[{"type":"text","text":"I misread the config key"}]}}"#,
            "\n",
            r#"{"uuid":"z1","type":"assistant","message":{"id":"m11","content":[{"type":"text","text":"I was wrong again."}]}}"#,
            "\n",
            r#"{"subtype":"compact_boundary","timestamp":"2026-10-10T00:01:00Z"}"#,
            "\n",
            r#"{"uuid":"f1","type":"assistant","message":{"id":"m12","content":[{"type":"text","text":"I stand corrected on the cache dir"}]}}"#,
            "\n",
        );
        let value = meter(raw);
        assert_eq!(value["windows_total"], json!(3));
        let recent = value["recent"].as_array().expect("recent array");
        assert_eq!(recent.len(), 2);
        assert_eq!(counts(&value, 0), (3, 1, 1, 1, 1));
        assert_eq!(counts(&value, 1), (1, 0, 0, 0, 0));
        assert_eq!(recent[0]["window"], json!(2));
        assert_eq!(recent[1]["window"], json!(3));

        // The acceptance pair verbatim: an unprompted retraction in window
        // 1, and the acknowledged correction pair in window 2.
        let ac1 = meter(concat!(
            r#"{"uuid":"a1","type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"I was wrong about the port."}]}}"#,
            "\n",
            r#"{"subtype":"compact_boundary","timestamp":"2026-10-10T00:00:00Z"}"#,
            "\n",
            r#"{"uuid":"a2","type":"user","message":{"content":"that's not true"}}"#,
            "\n",
            r#"{"uuid":"a3","type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"You're right, I was wrong."}]}}"#,
            "\n",
        ));
        assert_eq!(counts(&ac1, 0), (1, 0, 0, 0, 0));
        assert_eq!(counts(&ac1, 1), (0, 1, 0, 0, 0));
    }

    // The render names the open window and the previous closed one, drops
    // the previous clause with one window, and reads unmeasured rather
    // than rendering zeros.
    #[test]
    fn render_shapes() {
        let line = render_line(&json!({
            "windows_total": 2,
            "recent": [
                {"window": 1, "self_caught": 3, "operator_caught": 0, "peer_caught": 0, "interrupts": 0, "rejects": 0},
                {"window": 2, "self_caught": 0, "operator_caught": 1, "peer_caught": 0, "interrupts": 2, "rejects": 1},
            ],
        }));
        assert_eq!(
            line,
            "self_correction: window 2 (open) self 0 / operator 1 (interrupts 2, rejects 1); window 1 self 3 / operator 0"
        );
        let line = render_line(&json!({
            "windows_total": 1,
            "recent": [
                {"window": 1, "self_caught": 2, "operator_caught": 0, "peer_caught": 1, "interrupts": 0, "rejects": 0},
            ],
        }));
        assert_eq!(
            line,
            "self_correction: window 1 (open) self 2 / operator 0 (interrupts 0, rejects 0)"
        );
        assert_eq!(
            render_line(&json!({"unmeasured": "no compaction windows to count corrections in for harness codex"})),
            "self_correction: unmeasured (no compaction windows to count corrections in for harness codex)"
        );
        assert_eq!(
            render_line(&json!({"windows_total": 0, "recent": []})),
            "self_correction: unmeasured (no compaction window read)"
        );
    }
}

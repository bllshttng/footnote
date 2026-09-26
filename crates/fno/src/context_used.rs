//! The context-used reading behind a pane frame's `ctx` cell: how much of the
//! model's context window the session has spent. One question over the
//! transcript window the server already reads every tick ([`crate::transcript_tail::TailReader`]),
//! harness-blind:
//!
//! - codex rollouts end in `token_count` events carrying
//!   `info.model_context_window`, so the reading is a percent.
//! - claude transcripts carry assistant `message.usage` sums with NO window,
//!   so the reading is raw thousands of tokens (`98k`).
//!
//! Nothing is guessed: a tail with neither shape (or a malformed line) reads
//! `None` and the frame drops the field.

/// The most recent reading in a transcript tail window, newest line first.
/// `None` when neither harness shape appears.
pub(crate) fn from_tail(text: &str) -> Option<String> {
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"] == "token_count" {
            let used = v["info"]["last_token_usage"]["input_tokens"].as_u64();
            let window = v["info"]["model_context_window"].as_u64();
            if let (Some(used), Some(window)) = (used, window) {
                if window > 0 {
                    return Some(format!("{}%", used * 100 / window));
                }
            }
            continue;
        }
        if v["type"] == "assistant" {
            let u = &v["message"]["usage"];
            if let (Some(i), Some(cr), Some(cc)) = (
                u["input_tokens"].as_u64(),
                u["cache_read_input_tokens"].as_u64(),
                u["cache_creation_input_tokens"].as_u64(),
            ) {
                return Some(format!("{}k", (i + cr + cc) / 1000));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_COUNT: &str = r#"{"type":"token_count","info":{"total_token_usage":{"input_tokens":1},"last_token_usage":{"input_tokens":414200},"model_context_window":828400}}"#;
    const CLAUDE_TURN: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":420,"cache_read_input_tokens":97000,"cache_creation_input_tokens":1030}}}"#;

    // AC4-HP: codex window -> percent.
    #[test]
    fn codex_token_count_reads_a_percent() {
        assert_eq!(from_tail(CODEX_COUNT).as_deref(), Some("50%"));
    }

    // AC4-EDGE: claude window -> raw tokens, no window to divide by.
    #[test]
    fn claude_assistant_usage_reads_raw_thousands() {
        assert_eq!(from_tail(CLAUDE_TURN).as_deref(), Some("98k"));
    }

    // AC4-ERR: neither shape, or a broken line, reads None.
    #[test]
    fn no_usable_record_reads_none() {
        assert_eq!(from_tail(""), None);
        assert_eq!(from_tail("not json\n"), None);
        assert_eq!(
            from_tail(r#"{"type":"token_count","info":{}}"#),
            None,
            "a token_count with no window is not a percent"
        );
        assert_eq!(
            from_tail(&format!("{CLAUDE_TURN}\n{CODEX_COUNT}\n")).as_deref(),
            Some("50%"),
            "the newest record wins"
        );
    }
}

//! The zcode harness's identity half: the `sess_<uuid>` shape and the
//! stream-json turn parser. Measured live 2026-09-29 against ZCode.app 3.14.3:
//! a headless `zcode -p <prompt> --mode yolo --output-format stream-json`
//! turn emits one JSON event per line - every line carries `sessionId` of the
//! shape `sess_<uuid>`, and the final `result` line carries `response` and
//! `usage`. A second process `--resume <id>` recalls the first turn's
//! codeword. The interactive TUI form is NOT usable on that install: the app
//! bundle resolves no `@zcode/tui` beside zcode.cjs, so a TUI launch exits 1
//! before painting (captured twice, 2026-09-29); the capability row carries
//! that evidence rather than an unmeasured lane.

/// Cap on RETAINED stderr from a turn, the same shape as the stop gate's
/// bounded reads: the tail carries the line that killed the turn.
pub(crate) const TURN_STDERR_TAIL_CAP: usize = 2000;

/// zcode mints `sess_` plus a full UUID. The shape check is the point: a bare
/// UUID is a different harness's identity space, and a truncated tail
/// addresses nothing - neither may ever be read as a zcode identity.
pub fn is_session_id(value: &str) -> bool {
    value.len() == 41 && value.starts_with("sess_") && crate::pane_keeper::is_full_uuid(&value[5..])
}

/// One parsed headless turn: the session id the turn ran in, and the reply
/// the turn ended with. `session_id` is present on every measured line, so a
/// create turn binds its identity from the same stream that carries the work;
/// `reply` rides only the final `result` line, so an ask without one is an
/// incomplete turn, not a successful empty reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcodeTurn {
    pub session_id: Option<String>,
    pub reply: Option<String>,
}

/// Parse a headless turn's captured output. Exit status and the stderr tail
/// ride along so a refusal names what actually happened instead of "the read
/// failed". Split from the runner in `zcode_ask` so tests parse captured
/// output rather than running zcode.
pub fn parse_turn_output(
    ok: bool,
    code: Option<i32>,
    stdout: &str,
    stderr_tail: &str,
) -> Result<ZcodeTurn, String> {
    let detail = |what: &str| {
        format!(
            "zcode turn {}: exit_code={} stderr_tail={:?}",
            what,
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            tail_text(stderr_tail)
        )
    };
    if !ok {
        return Err(detail("failed"));
    }
    let mut session_id: Option<String> = None;
    let mut reply: Option<String> = None;
    for line in stdout.lines() {
        // Noise lines (a chatty build, a stray ^D) are skipped, not fatal:
        // identity and reply only bind from lines that parse.
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if session_id.is_none() {
            if let Some(id) = event.get("sessionId").and_then(|v| v.as_str()) {
                if is_session_id(id) {
                    session_id = Some(id.to_string());
                }
            }
        }
        if event.get("type").and_then(|v| v.as_str()) == Some("result") {
            reply = event
                .get("response")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
    }
    match session_id {
        Some(id) => Ok(ZcodeTurn {
            session_id: Some(id),
            reply,
        }),
        None => Err(detail("printed no sessionId in the zcode shape")),
    }
}

fn tail_text(stderr_tail: &str) -> String {
    let chars: Vec<char> = stderr_tail.chars().collect();
    let start = chars.len().saturating_sub(TURN_STDERR_TAIL_CAP);
    chars[start..].iter().collect()
}

use crate::provider::CreateContext;
use crate::provider::Provider;
use crate::provider::ReachabilityProbeError;
use crate::provider::ResumeContext;
use crate::ParsedEvent;
use std::time::Duration;

pub struct ZcodeProvider;

impl Provider for ZcodeProvider {
    fn name(&self) -> &'static str {
        "zcode"
    }

    fn create_argv(&self, _ctx: &CreateContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv("zcode", "headless_create", None)
            .expect("embedded zcode headless-create capability")
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv(
            "zcode",
            "headless_resume",
            Some(&ctx.session_id),
        )
        .expect("embedded zcode headless-resume capability")
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(chunk.trim()) else {
            return ParsedEvent::Unknown {
                raw: chunk.to_string(),
            };
        };
        if event.get("type").and_then(|v| v.as_str()) == Some("result") {
            if let Some(text) = event.get("response").and_then(|v| v.as_str()) {
                return ParsedEvent::ReplyComplete {
                    text: text.to_string(),
                    duration_ms: 0,
                };
            }
        }
        ParsedEvent::Unknown {
            raw: chunk.to_string(),
        }
    }

    fn reachability(
        &self,
        _entry: &crate::provider::AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        Err(ReachabilityProbeError::new(
            "zcode",
            "session store is zcode's own db; reachability requires live recall",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "sess_60de086c-9278-4b56-addb-39445b2e6636";

    #[test]
    fn identity_shape_is_exactly_sess_plus_uuid() {
        assert!(is_session_id(ID));
        // A bare UUID is another harness's identity space; a truncated tail
        // addresses nothing; empty is nothing.
        assert!(!is_session_id("60de086c-9278-4b56-addb-39445b2e6636"));
        assert!(!is_session_id("sess_60de086c-9278-4b56-addb-39445b2e"));
        assert!(!is_session_id(""));
        assert!(!is_session_id("sess_"));
    }

    #[test]
    fn the_captured_stream_binds_identity_and_reply() {
        // Real event shapes from the 2026-09-29 capture, condensed: identity
        // on every line, the reply on the final result line.
        let out = format!(
            "{{\"type\":\"turn.started\",\"sessionId\":\"{ID}\",\"seq\":2}}\n\
             {{\"type\":\"model.streaming\",\"sessionId\":\"{ID}\",\"seq\":9}}\n\
             {{\"type\":\"result\",\"sessionId\":\"{ID}\",\"response\":\"PONG\",\"usage\":{{}}}}\n"
        );
        let turn = parse_turn_output(true, Some(0), &out, "").expect("the captured stream parses");
        assert_eq!(turn.session_id.as_deref(), Some(ID));
        assert_eq!(turn.reply.as_deref(), Some("PONG"));
    }

    #[test]
    fn a_seed_turn_binds_identity_without_needing_a_reply() {
        let out = format!("{{\"type\":\"turn.started\",\"sessionId\":\"{ID}\"}}\n");
        let turn =
            parse_turn_output(true, Some(0), &out, "").expect("identity alone is a valid turn");
        assert_eq!(turn.session_id.as_deref(), Some(ID));
        assert_eq!(turn.reply, None);
    }

    #[test]
    fn unusable_streams_refuse_and_name_the_cause() {
        // No parseable identity anywhere.
        let err = parse_turn_output(true, Some(0), "no json here\n", "")
            .expect_err("no sessionId is a refusal");
        assert!(err.contains("no sessionId"), "{err}");
        // An off-shape id never binds.
        let out = "{\"type\":\"result\",\"sessionId\":\"short\",\"response\":\"PONG\"}\n";
        let err = parse_turn_output(true, Some(0), out, "").expect_err("off-shape id refuses");
        assert!(err.contains("no sessionId"), "{err}");
        // A failed turn names its exit and stderr.
        let err = parse_turn_output(false, Some(1), "", "Select a model before continuing")
            .expect_err("a failed turn refuses");
        assert!(err.contains("exit_code=1"), "{err}");
        assert!(err.contains("Select a model"), "{err}");
    }

    #[test]
    fn the_provider_renders_the_headless_forms() {
        let ctx = CreateContext {
            name: "zc".into(),
            message: "seed".into(),
            cwd: std::env::temp_dir(),
            from_name: None,
            session_id: None,
            yolo: true,
            reasoning_effort: None,
            append_system_prompt: None,
        };
        let argv = ZcodeProvider.create_argv(&ctx);
        assert_eq!(argv[0], "zcode");
        assert!(argv.contains(&"--output-format".to_string()));
        let rctx = ResumeContext {
            session_id: ID.to_string(),
            message: "go".into(),
            cwd: std::env::temp_dir(),
            from_name: None,
            yolo: true,
        };
        let argv = ZcodeProvider.resume_argv(&rctx);
        let at = argv
            .iter()
            .position(|a| a == "--resume")
            .expect("resume flag");
        assert_eq!(argv[at + 1], ID);
    }

    #[test]
    fn the_provider_maps_result_lines_and_refuses_nothing() {
        let result_line =
            format!("{{\"type\":\"result\",\"sessionId\":\"{ID}\",\"response\":\"PONG\"}}");
        match ZcodeProvider.parse_stream_event(&result_line) {
            ParsedEvent::ReplyComplete { text, .. } => assert_eq!(text, "PONG"),
            other => panic!("want ReplyComplete, got {other:?}"),
        }
        assert!(matches!(
            ZcodeProvider.parse_stream_event("not json"),
            ParsedEvent::Unknown { .. }
        ));
    }
}

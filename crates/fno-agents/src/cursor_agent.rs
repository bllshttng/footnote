//! Cursor Agent's pane-only session identity.
//!
//! `cursor-agent create-chat` mints a UUID before the interactive process
//! starts, then stays alive instead of exiting. fno reads one line and kills
//! that helper; the pane launches with the full id on `--resume`. Cursor's
//! transcript store is remote, so reachability cannot be inferred from a local
//! session file.

use std::time::Duration;

use crate::provider::{AgentEntry, Provider, ReachabilityProbeError, ResumeContext};
use crate::ParsedEvent;

pub const CURSOR_AGENT_BINARY: &str = "cursor-agent";
pub const CURSOR_AGENT_DEFAULT_PROVIDER: &str = "cursor";
pub const CURSOR_AGENT_DEFAULT_MODEL: &str = "auto";

pub fn cursor_agent_provider() -> String {
    std::env::var("FNO_CURSOR_AGENT_PROVIDER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| CURSOR_AGENT_DEFAULT_PROVIDER.to_string())
}

pub fn cursor_agent_model() -> String {
    std::env::var("FNO_CURSOR_AGENT_MODEL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| CURSOR_AGENT_DEFAULT_MODEL.to_string())
}

pub fn is_chat_id(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts
            .iter()
            .all(|part| part.chars().all(|c| c.is_ascii_hexdigit()))
        && parts[2].starts_with('4')
        && matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'A' | 'b' | 'B')
        )
}

pub fn chat_id_error(value: &str) -> Option<String> {
    if value.len() == 8 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!(
            "cursor-agent chat id '{value}' is 8 hex characters, which is an fno session handle, not a chat id.\nPass the full UUID that `cursor-agent create-chat` returned."
        ));
    }
    if value.is_empty() {
        return Some(
            "cursor-agent chat id is empty; pass the full UUID that `cursor-agent create-chat` returned."
                .to_string(),
        );
    }
    (!is_chat_id(value)).then(|| {
        format!(
            "cursor-agent chat id {value:?} is not a full UUIDv4 returned by `cursor-agent create-chat`."
        )
    })
}

pub fn attach_argv(chat_id: &str) -> Vec<String> {
    if let Some(error) = chat_id_error(chat_id) {
        panic!("{error}");
    }
    crate::harness_capabilities::render_session_argv(
        "cursor-agent",
        "interactive_attach",
        Some(chat_id),
    )
    .expect("embedded cursor-agent interactive-attach capability")
}

/// Cursor's pane is interactive, but it has no Rust-owned local drive lane.
/// The provider exists for roster and attach parity; Python owns pane launch.
pub struct CursorAgentProvider;

impl Provider for CursorAgentProvider {
    fn name(&self) -> &'static str {
        "cursor-agent"
    }

    fn create_argv(&self, ctx: &crate::provider::CreateContext) -> Vec<String> {
        let mut argv = crate::harness_capabilities::render_session_argv(
            "cursor-agent",
            "interactive_create",
            ctx.session_id.as_deref(),
        )
        .expect("embedded cursor-agent interactive-create capability");
        argv.push("--trust".to_string());
        if let Some(model) = std::env::var("FNO_CURSOR_AGENT_MODEL")
            .ok()
            .filter(|value| !value.is_empty())
        {
            argv.extend(["--model".to_string(), model]);
        }
        argv
    }

    fn resume_argv(&self, ctx: &ResumeContext) -> Vec<String> {
        let mut argv = attach_argv(&ctx.session_id);
        argv.extend(["--".to_string(), ctx.message.clone()]);
        argv
    }

    fn parse_stream_event(&self, chunk: &str) -> ParsedEvent {
        if chunk.trim().is_empty() {
            ParsedEvent::Unknown {
                raw: chunk.to_string(),
            }
        } else {
            ParsedEvent::ReplyComplete {
                text: chunk.to_string(),
                duration_ms: 0,
            }
        }
    }

    fn reachability(
        &self,
        entry: &AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, ReachabilityProbeError> {
        let _session_id = entry
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ReachabilityProbeError::new("cursor-agent", "no chat id in entry"))?;
        Err(ReachabilityProbeError::new(
            "cursor-agent",
            "chat store is remote; reachability requires live pane cross-process recall",
        ))
    }
}

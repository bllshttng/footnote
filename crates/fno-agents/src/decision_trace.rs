//! The traced-decision envelope: one `decision_span` row per hop in a
//! cross-rung decision (worker ask, lead route, correction, guard block),
//! written to the project journal. Later surfaces (reap, merge, gate
//! override) adopt the same envelope by adding a `span_kind` value, never a
//! new event type. Shape and reader queries: docs/architecture/decision-record.md.

use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::Path;

/// Who acted on a span. `lead` needs a live team bound to the session,
/// `worker` is any other resolved session, `user` is an attended terminal
/// with no session identity (the decide door's state 2), `sweep` is daemon
/// work. The same vocabulary names a span's recipient.
pub const ACTOR_KINDS: &[&str] = &["user", "lead", "worker", "sweep"];

/// The comms surface a span traveled on: the tag the Notifications tab
/// groups by. `mail` is the agent mail bus, `chat` the harness transcript,
/// `question` the operator question lane.
pub const COMMS_KINDS: &[&str] = &["mail", "chat", "question"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trace {
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_session: Option<String>,
    pub actor_kind: &'static str,
    pub comms: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient_kind: Option<&'static str>,
}

pub fn actor_kind(session: Option<&str>, source: &str) -> &'static str {
    if source == "daemon" {
        return "sweep";
    }
    let Some(handle) = session else {
        return "user";
    };
    let handle = crate::identity::canonical_handle(handle);
    let teamed =
        crate::territory::live_teams(&crate::paths::AgentsHome::from_env().registry_json())
            .map(|teams| {
                teams.iter().any(|team| {
                    team.holder_session
                        .as_deref()
                        .is_some_and(|s| crate::identity::canonical_handle(s) == handle)
                })
            })
            // A registry read error reads as not teamed (fail closed), the
            // same reading question_clear's caller resolution takes.
            .unwrap_or(false);
    if teamed {
        "lead"
    } else {
        "worker"
    }
}

/// The span id mint for hops with no natural id of their own. A row that
/// already carries one (question_id, decision_id) reuses it instead. On
/// entropy failure the id mixes clock and pid rather than emitting
/// colliding all-zero ids; a span id is a journal row handle, not the
/// session identity mint_fno_id guards.
pub fn new_span_id() -> String {
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        b = (nanos as u32 ^ std::process::id()).to_le_bytes();
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("s-{hex}")
}

pub fn emit_span(span_kind: &str, trace: &Trace, attrs: &Map<String, Value>) -> Result<(), String> {
    emit_span_to(
        &crate::law_match::project_events_journal(),
        span_kind,
        trace,
        attrs,
    )
}

pub fn emit_span_to(
    journal: &Path,
    span_kind: &str,
    trace: &Trace,
    attrs: &Map<String, Value>,
) -> Result<(), String> {
    let mut data = Map::new();
    data.insert("span_kind".to_string(), json!(span_kind));
    let trace_value =
        serde_json::to_value(trace).map_err(|e| format!("decision_span trace: {e}"))?;
    data.insert("trace".to_string(), trace_value);
    for (k, v) in attrs {
        data.insert(k.clone(), v.clone());
    }
    let envelope = json!({
        "ts": crate::events::now_rfc3339(),
        "type": "decision_span",
        "source": "target",
        "data": data,
    });
    crate::event_store::append_envelope(journal, &envelope.to_string(), None).map(|_| ())
}

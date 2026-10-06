//! `fno-agents hook send-message-guard` - the native SendMessage PreToolUse
//! guard.
//!
//! Peer messages between fno sessions ride `fno agents mail send`, not the
//! harness's native SendMessage: a native send never reaches the mail journal,
//! so the feed, the Messages tab, check-in counts and provenance all miss it.
//! When the target resolves to an fno-registered session this guard denies
//! with a redirect to `fno agents mail send`; it allows when the target is not
//! a registry row (an in-process teammate, `main`, a cloud session) and when
//! fno mail itself is failing - the outage fallback, journalled as a
//! `sendmessage_fallback` event so provenance survives the outage.
//!
//! Any failure to READ (payload, registry, bus) allows - the never-block
//! contract every hook here shares.

use serde_json::{json, Value};

/// The guard's one decision, pure so the unit test pins it without stdin.
enum Decision {
    /// Not a registry row (teammate, `main`, cloud session): native lane.
    AllowNotRegistered,
    /// A registry row, and mail is healthy: deny with the mail redirect.
    Deny,
    /// A registry row, and mail is failing: the outage fallback, journalled.
    AllowFallback(String),
}

fn decide(to: &str, rows: &[Value], unhealthy: Option<String>) -> Decision {
    if to.is_empty() || crate::mail_threads::registry_lookup(rows, to).is_none() {
        return Decision::AllowNotRegistered;
    }
    match unhealthy {
        Some(why) => Decision::AllowFallback(why),
        None => Decision::Deny,
    }
}

/// Entry: read the payload once, decide, print, always exit 0.
pub fn run(_args: &[String]) -> i32 {
    let trace = std::env::var_os("FNO_GUARD_TRACE").is_some();
    let allow_at = |stage: &str| -> i32 {
        if trace {
            eprintln!("send-message-guard: allow at {stage}");
        }
        super::emit_allow()
    };
    let raw: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
    if raw.is_null() {
        return allow_at("payload-null");
    }
    let to = raw
        .pointer("/tool_input/to")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if to.is_empty() {
        return allow_at("no-target");
    }
    let rows = registry_rows();
    if rows.is_empty() {
        return allow_at("registry-empty");
    }
    match decide(&to, &rows, mail_unhealthy()) {
        Decision::AllowNotRegistered => allow_at("not-registered"),
        Decision::Deny => {
            super::emit_guard_decision(
                &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                "send-message-guard",
                "SendMessage",
                true,
            );
            super::emit_block(&format!(
                "use fno mail, not SendMessage: `{to}` is an fno-registered session, \
                 and a native send never reaches the mail journal (feed, Messages tab, \
                 provenance all miss it). Send with: \
                 fno agents mail send \"{to}\" \"<message>\" --subject \"<one line>\" \
                 (the receipt is a JSON {{msg_id, subject, to, status}} line). \
                 SendMessage is the outage fallback only, and fno mail is healthy."
            ))
        }
        Decision::AllowFallback(why) => {
            journal_fallback(&to, &why, &raw);
            if trace {
                eprintln!("send-message-guard: outage fallback ({why})");
            }
            super::emit_allow()
        }
    }
}

/// The registry's rows, tolerant: a missing or malformed file reads as none.
fn registry_rows() -> Vec<Value> {
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return Vec::new();
    };
    let text = std::fs::read_to_string(home.registry_json()).unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("agents").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// Why fno mail is failing, when it is. Two concrete failure modes: the
/// registry unreadable (the 2026-10-06 crash that started this lane) and the
/// bus log not appendable (the durable write every send depends on). A
/// missing file is health, not failure - the first write creates it.
fn mail_unhealthy() -> Option<String> {
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return Some("agents home unresolved".to_string());
    };
    let reg = home.registry_json();
    if reg.exists() {
        let text = std::fs::read_to_string(&reg).unwrap_or_default();
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.get("agents").is_some() => {}
            _ => return Some(format!("registry unreadable: {}", reg.display())),
        }
    }
    let dot_fno = home.root().parent().map(|p| p.to_path_buf());
    let bus: std::path::PathBuf = dot_fno
        .as_deref()
        .map(crate::intel::bus_log_path)
        .unwrap_or_default();
    if bus.exists() {
        match std::fs::OpenOptions::new().append(true).open(&bus) {
            Ok(_) => {}
            Err(e) => return Some(format!("bus log not writable: {e}")),
        }
    }
    None
}

/// One `sendmessage_fallback` event row: the outage fallback's provenance.
/// Best-effort; an events-write failure never blocks the allowed send.
fn journal_fallback(to: &str, why: &str, payload: &Value) {
    let cwd = &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let path =
        crate::state_path::resolve("events", cwd).unwrap_or_else(|| crate::paths::events_path(cwd));
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let event = json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "type": "sendmessage_fallback",
        "data": {
            "to": to,
            "reason": why,
            "session_id": if session.is_empty() { Value::Null } else { json!(session) },
        },
        "source": "hook"
    });
    let _ = crate::claims::append_event_line(&path, &event, std::time::Duration::from_secs(2));
    super::emit_guard_decision(cwd, "send-message-guard", "SendMessage", false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_routes_by_registration_then_mail_health() {
        let rows = vec![
            json!({"name": "vellum", "fno_id": "s-vellum", "harness_session_id": "s-vellum"}),
            json!({"name": "candor", "fno_id": "s-candor", "aliases": ["cor"]}),
        ];
        // A registry name denies when mail is healthy.
        assert!(matches!(decide("vellum", &rows, None), Decision::Deny));
        // An alias denies too.
        assert!(matches!(decide("cor", &rows, None), Decision::Deny));
        // A session id denies.
        assert!(matches!(decide("s-candor", &rows, None), Decision::Deny));
        // A teammate name, `main`, or an in-process agent id allows.
        assert!(matches!(
            decide("main", &rows, None),
            Decision::AllowNotRegistered
        ));
        assert!(matches!(
            decide("researcher", &rows, None),
            Decision::AllowNotRegistered
        ));
        assert!(matches!(
            decide("", &rows, None),
            Decision::AllowNotRegistered
        ));
        // A registry row with mail failing is the journalled outage fallback.
        assert!(matches!(
            decide("vellum", &rows, Some("registry unreadable".into())),
            Decision::AllowFallback(_)
        ));
    }

    #[test]
    fn unhealthy_reads_name_the_two_failure_modes() {
        // (from_env_opt resolves through the env; a bare test env has one, so
        // this only asserts the function answers without panicking.)
        let _ = mail_unhealthy();
    }
}

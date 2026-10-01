use serde_json::{Map, Value};
use std::path::Path;

/// Commit one event row to the agents journal's store with the Python-agents
/// envelope (`{ts, type, source, data}`, the unified shape
/// `agents.events.emit` writes). Best-effort: on a store refusal it warns to
/// stderr and returns, mirroring `agents.events.emit` so a failed telemetry
/// write never blocks the primary command (AC1-FR).
///
/// Deliberately a free function (not a `.emit()` method) so the crate's
/// production-emit-kind scanner (which keys on `.emit(`/`.emit_fields(`) does
/// not treat these Python-side audit kinds as Rust daemon event kinds.
pub(crate) fn append_agents_event(events_path: &Path, kind: &str, fields: &[(&str, Value)]) {
    let mut data = Map::new();
    for (key, value) in fields {
        data.insert((*key).to_string(), value.clone());
    }
    if matches!(std::env::var("FNO_CALLER_KIND").as_deref(), Ok("mux"))
        && !data.contains_key("caller_kind")
    {
        data.insert("caller_kind".into(), Value::String("mux".into()));
    }
    let event = serde_json::json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "type": kind,
        "source": "agents",
        "data": Value::Object(data),
    });
    if let Err(exc) = crate::event_store::append_envelope(events_path, &event.to_string(), None) {
        eprintln!(
            "fno agents: warning: events.emit('{kind}') to {}: {exc}",
            events_path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    struct CallerKindEnv {
        previous: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl CallerKindEnv {
        fn set(value: Option<&str>) -> Self {
            let lock = crate::claims::test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("FNO_CALLER_KIND");
            match value {
                Some(value) => std::env::set_var("FNO_CALLER_KIND", value),
                None => std::env::remove_var("FNO_CALLER_KIND"),
            }
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for CallerKindEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("FNO_CALLER_KIND", value),
                None => std::env::remove_var("FNO_CALLER_KIND"),
            }
        }
    }

    fn event_line(fields: &[(&str, Value)]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events.jsonl");
        append_agents_event(&events, "agent_resumed", fields);
        let rows = crate::event_store::query_events(
            &events,
            &crate::event_store::EventQuery::of_types(&["agent_resumed"]),
        )
        .expect("the store beside the journal opened");
        (
            dir,
            rows.into_iter().next().expect("the row committed").line,
        )
    }

    #[test]
    fn append_agents_event_writes_python_envelope() {
        let _env = CallerKindEnv::set(None);
        let (_dir, content) = event_line(&[
            ("name", Value::String("worker-A".into())),
            ("provider", Value::String("codex".into())),
        ]);
        let line = content.trim_end();
        let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(parsed["type"], "agent_resumed");
        assert_eq!(parsed["source"], "agents");
        assert_eq!(parsed["data"]["name"], "worker-A");
        assert_eq!(parsed["data"]["provider"], "codex");
    }

    #[test]
    fn append_agents_event_stamps_mux_caller_kind_without_overwriting_fields() {
        let _env = CallerKindEnv::set(Some("mux"));
        let fields = [
            ("name", Value::String("worker-A".into())),
            ("provider", Value::String("codex".into())),
        ];
        let (_dir, content) = event_line(&fields);
        let line = content.trim_end();
        let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(parsed["data"]["caller_kind"], "mux");
        assert_eq!(line.matches("\"caller_kind\":").count(), 1);

        let (_dir, explicit_content) =
            event_line(&[("caller_kind", Value::String("explicit".into()))]);
        let explicit_line = explicit_content.trim_end();
        let explicit: Value = serde_json::from_str(explicit_line).expect("valid JSON line");
        assert_eq!(explicit["data"]["caller_kind"], "explicit");
        assert_eq!(explicit_line.matches("\"caller_kind\":").count(), 1);
    }

    #[test]
    fn append_agents_event_keeps_envelope_for_unset_or_other_caller_kind() {
        for caller_kind in [None, Some("human_cli")] {
            let _env = CallerKindEnv::set(caller_kind);
            let (_dir, content) = event_line(&[
                ("name", Value::String("worker-A".into())),
                ("provider", Value::String("codex".into())),
            ]);
            let parsed: Value = serde_json::from_str(content.trim_end()).unwrap();
            assert_eq!(parsed["type"], "agent_resumed");
            assert_eq!(parsed["source"], "agents");
            assert_eq!(parsed["data"]["name"], "worker-A");
            assert_eq!(parsed["data"]["provider"], "codex");
            assert!(parsed
                .get("data")
                .and_then(|d| d.get("caller_kind"))
                .is_none());
        }
    }
}

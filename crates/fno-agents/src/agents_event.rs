use serde_json::Value;
use std::path::Path;

/// Append one event line to `state_dir/events.jsonl` with the Python-agents
/// envelope (`{...fields, ts, kind}`, compact). Best-effort: on a write error
/// it warns to stderr and returns, mirroring `agents.events.emit` so a failed
/// telemetry write never blocks the primary command (AC1-FR).
///
/// Deliberately a free function (not a `.emit()` method) so the crate's
/// production-emit-kind scanner (which keys on `.emit(`/`.emit_fields(`) does
/// not treat these Python-side audit kinds as Rust daemon event kinds.
pub(crate) fn append_agents_event(events_path: &Path, kind: &str, fields: &[(&str, Value)]) {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}:{}",
                serde_json::to_string(k).unwrap_or_default(),
                serde_json::to_string(v).unwrap_or_default()
            )
        })
        .collect();
    if matches!(std::env::var("FNO_CALLER_KIND").as_deref(), Ok("mux"))
        && !fields.iter().any(|(key, _)| *key == "caller_kind")
    {
        parts.push(format!(
            "\"caller_kind\":{}",
            serde_json::to_string("mux").unwrap_or_default()
        ));
    }
    parts.push(format!(
        "\"ts\":{}",
        serde_json::to_string(&ts).unwrap_or_default()
    ));
    parts.push(format!(
        "\"kind\":{}",
        serde_json::to_string(kind).unwrap_or_default()
    ));
    let line = format!("{{{}}}\n", parts.join(","));

    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = events_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path)?;
        fh.write_all(line.as_bytes())
    })();
    if let Err(exc) = result {
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
        let content = std::fs::read_to_string(events).unwrap();
        (dir, content)
    }

    #[test]
    fn append_agents_event_writes_python_envelope() {
        let _env = CallerKindEnv::set(None);
        let (_dir, content) = event_line(&[
            ("name", Value::String("worker-A".into())),
            ("provider", Value::String("codex".into())),
        ]);
        let line = content.trim_end();
        assert!(line.starts_with(r#"{"name":"worker-A","provider":"codex","ts":"#));
        assert!(line.ends_with(r#""kind":"agent_resumed"}"#));
        let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(parsed["kind"], "agent_resumed");
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
        assert_eq!(parsed["caller_kind"], "mux");
        assert_eq!(line.matches("\"caller_kind\":").count(), 1);

        let (_dir, explicit_content) =
            event_line(&[("caller_kind", Value::String("explicit".into()))]);
        let explicit_line = explicit_content.trim_end();
        let explicit: Value = serde_json::from_str(explicit_line).expect("valid JSON line");
        assert_eq!(explicit["caller_kind"], "explicit");
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
            let ts = parsed["ts"].as_str().unwrap();
            assert_eq!(
                content,
                format!(
                    "{{\"name\":\"worker-A\",\"provider\":\"codex\",\"ts\":\"{ts}\",\"kind\":\"agent_resumed\"}}\n"
                )
            );
        }
    }
}

//! The `rename` verb's client half: argv-to-request shaping and the success
//! receipt. The daemon owns the transaction, except `--from-journal`, the
//! daemon-free label rebuild that reads the registry and journal directly.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::event_store::EventRow;
use crate::state::RegistryEntry;

/// One row's planned label rebuild.
pub struct Rebuild {
    /// The label the registry holds today.
    pub name: String,
    /// The full harness session id the rename is keyed by (d-e952ed19).
    pub session: String,
    /// The newest label the journal names for that session.
    pub journal_name: String,
    /// Set when the rebuild is refused, so `--apply` renames the rest.
    pub reason: Option<String>,
}

/// Pure: the label rebuild each row needs. Events are keyed by
/// `data.harness_session_id`, newest by `ts_ms` then `seq`; `agent_spawned`
/// contributes `data.name` and `agent_renamed` contributes `data.to`. A row
/// already named as its journal plans nothing; a journal name another row
/// holds plans a skip with the reason.
pub fn plan_from_journal(rows: &[RegistryEntry], events: &[EventRow]) -> Vec<Rebuild> {
    let mut newest: BTreeMap<String, (String, i64, i64)> = BTreeMap::new();
    for row in events {
        let Ok(value) = serde_json::from_str::<Value>(&row.line) else {
            continue;
        };
        let Some(data) = value.get("data") else {
            continue;
        };
        let Some(session) = data
            .get("harness_session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let label = match row.r#type.as_str() {
            "agent_spawned" => data.get("name").and_then(Value::as_str),
            "agent_renamed" => data.get("to").and_then(Value::as_str),
            _ => continue,
        };
        let Some(label) = label.filter(|l| !l.is_empty()) else {
            continue;
        };
        let newer = newest
            .get(session)
            .map_or(true, |(_, ts, seq)| (*ts, *seq) < (row.ts_ms, row.seq));
        if newer {
            newest.insert(session.to_string(), (label.to_string(), row.ts_ms, row.seq));
        }
    }
    let mut plans = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(session) = row.harness_session_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some((label, _, _)) = newest.get(session) else {
            continue;
        };
        let label = label.as_str();
        if label == row.name {
            continue;
        }
        let taken = rows
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && other.name == label);
        plans.push(Rebuild {
            name: row.name.clone(),
            session: session.to_string(),
            journal_name: label.to_string(),
            reason: taken.then(|| format!("registry label {label:?} already names another row")),
        });
    }
    plans
}

/// `rename --from-journal`: print one `<current> -> <journal name>` line per
/// row (a refused row prints its skip reason) and write nothing without
/// `--apply`. With `--apply` each planned row renames through
/// `state::rename_agent`, keyed by the full session id. Exit 0, or 1 when
/// any row was refused.
pub fn run_from_journal(args: &[String], home: &crate::paths::AgentsHome) -> i32 {
    let apply = args.iter().any(|a| a == "--apply");
    let registry_path = home.registry_json();
    let registry = match crate::state::load_registry(&registry_path) {
        Ok(registry) => registry,
        Err(e) => {
            eprintln!(
                "rename --from-journal: cannot read {}: {e}",
                registry_path.display()
            );
            return 2;
        }
    };
    let _ = crate::event_store::import_all(&home.events_jsonl());
    let query = crate::event_store::EventQuery {
        types: vec!["agent_spawned".into(), "agent_renamed".into()],
        ..Default::default()
    };
    let events = match crate::event_store::query_events(&home.events_jsonl(), &query) {
        Ok(events) => events,
        Err(e) => {
            eprintln!("rename --from-journal: cannot read the agents journal: {e}");
            return 2;
        }
    };
    let plans = plan_from_journal(&registry.entries, &events);
    let mut refused = 0;
    for plan in &plans {
        match &plan.reason {
            Some(reason) => {
                println!("skip {} -> {}: {reason}", plan.name, plan.journal_name);
                refused += 1;
            }
            None => println!("{} -> {}", plan.name, plan.journal_name),
        }
    }
    if plans.is_empty() {
        println!("rename --from-journal: every label already matches the journal");
    }
    if !apply {
        return i32::from(refused > 0);
    }
    for plan in &plans {
        if plan.reason.is_some() {
            continue;
        }
        if let Err(e) =
            crate::state::rename_agent(&registry_path, &plan.session, &plan.journal_name, None)
        {
            eprintln!("rename {} -> {} failed: {e}", plan.name, plan.journal_name);
            refused += 1;
        }
    }
    i32::from(refused > 0)
}

/// Shape `rename <token> --name <new-label>`: swap the `--name` value (which
/// lands in `params.name`) with the positional token, carrying the label as
/// `new_name`.
pub fn request(params: &mut Map<String, Value>, positional: &[String]) -> Result<(), String> {
    let new_name = params
        .remove("name")
        .ok_or("rename needs --name <new-label>")?;
    let token = positional
        .first()
        .ok_or("rename needs a <name> to rename")?;
    params.insert("new_name".into(), new_name);
    params.insert("name".into(), Value::String(token.clone()));
    Ok(())
}

/// The receipt names BOTH labels: the new one is the live address, the old one
/// the alias a later `peek`/`mail send` may still use.
pub fn receipt(name: &str, result: &Value) -> Option<String> {
    let old = result
        .get("old_name")
        .and_then(Value::as_str)
        .unwrap_or(name);
    let new = result
        .get("new_name")
        .and_then(Value::as_str)
        .unwrap_or("(unknown)");
    Some(format!("renamed {old} -> {new}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AgentsHome;
    use std::path::PathBuf;

    fn row(name: &str, session: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            harness: Some("claude".into()),
            harness_session_id: Some(session.into()),
            ..Default::default()
        }
    }

    fn spawned(session: &str, name: &str, ts_ms: i64, seq: i64) -> EventRow {
        EventRow {
            seq,
            event_id: format!("e{seq}"),
            ts_ms,
            r#type: "agent_spawned".into(),
            source: "daemon".into(),
            scope: None,
            retention_class: "durable".into(),
            reject_reason: None,
            line: serde_json::json!({
                "ts": "2026-09-27T00:00:00Z",
                "type": "agent_spawned",
                "source": "daemon",
                "data": {"name": name, "harness_session_id": session},
            })
            .to_string(),
        }
    }

    fn renamed(session: &str, to: &str, ts_ms: i64, seq: i64) -> EventRow {
        EventRow {
            seq,
            event_id: format!("e{seq}"),
            ts_ms,
            r#type: "agent_renamed".to_string(),
            source: "daemon".into(),
            scope: None,
            retention_class: "durable".into(),
            reject_reason: None,
            line: serde_json::json!({
                "ts": "2026-09-27T00:00:00Z",
                "type": "agent_renamed",
                "source": "daemon",
                "data": {"to": to, "harness_session_id": session},
            })
            .to_string(),
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-rename-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn plan_maps_the_journal_label() {
        let rows = vec![row("05dddd48", "sess-1")];
        let events = vec![spawned("sess-1", "vellum", 100, 1)];
        let plans = plan_from_journal(&rows, &events);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].name, "05dddd48");
        assert_eq!(plans[0].session, "sess-1");
        assert_eq!(plans[0].journal_name, "vellum");
        assert!(plans[0].reason.is_none());
    }

    #[test]
    fn plan_takes_the_newest_journal_label() {
        let rows = vec![row("05dddd48", "sess-1")];
        let events = vec![
            renamed("sess-1", "first", 100, 2),
            renamed("sess-1", "vellum", 100, 3),
            renamed("sess-1", "older", 50, 9),
        ];
        let plans = plan_from_journal(&rows, &events);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].journal_name, "vellum", "newest ts then seq wins");
    }

    #[test]
    fn plan_skips_a_name_another_row_holds() {
        let rows = vec![row("05dddd48", "sess-1"), row("vellum", "sess-2")];
        let events = vec![spawned("sess-1", "vellum", 100, 1)];
        let plans = plan_from_journal(&rows, &events);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].reason.is_some(), "the held name is named");
    }

    #[test]
    fn plan_leaves_a_matching_row_alone() {
        let rows = vec![row("vellum", "sess-2")];
        let events = vec![spawned("sess-2", "vellum", 100, 1)];
        assert!(plan_from_journal(&rows, &events).is_empty());
    }

    #[test]
    fn run_from_journal_apply_renames_and_skips() {
        let dir = tmpdir("apply-happy");
        let home = AgentsHome::at(&dir);
        let path = home.registry_json();
        crate::state::update_registry(&path, |registry| {
            registry.entries = vec![row("05dddd48", "sess-1")];
        })
        .unwrap();
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        emitter
            .emit(
                "agent_spawned",
                &serde_json::json!({"name": "vellum", "harness_session_id": "sess-1"}),
            )
            .unwrap();

        let code = run_from_journal(
            &[
                "rename".to_string(),
                "--from-journal".into(),
                "--apply".into(),
            ],
            &home,
        );

        assert_eq!(code, 0);
        let registry = crate::state::load_registry(&path).unwrap();
        let renamed_row = registry
            .entries
            .iter()
            .find(|e| e.name == "vellum")
            .unwrap();
        assert_eq!(renamed_row.harness_session_id.as_deref(), Some("sess-1"));
        assert!(renamed_row.aliases.iter().any(|a| a == "05dddd48"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_from_journal_apply_skips_held_name_and_exits_1() {
        let dir = tmpdir("apply-held");
        let home = AgentsHome::at(&dir);
        let path = home.registry_json();
        crate::state::update_registry(&path, |registry| {
            registry.entries = vec![
                row("05dddd48", "sess-1"),
                row("99887766", "aaaaaaaa-0000-0000-0000-222222222222"),
                row("vellum", "sess-3"),
            ];
        })
        .unwrap();
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        emitter
            .emit(
                "agent_spawned",
                &serde_json::json!({"name": "vellum", "harness_session_id": "sess-1"}),
            )
            .unwrap();
        emitter
            .emit(
                "agent_spawned",
                &serde_json::json!({
                    "name": "ghost",
                    "harness_session_id": "aaaaaaaa-0000-0000-0000-222222222222"
                }),
            )
            .unwrap();

        let code = run_from_journal(
            &[
                "rename".to_string(),
                "--from-journal".into(),
                "--apply".into(),
            ],
            &home,
        );

        // sess-1's journal name is held by the sess-3 row: skipped with a
        // named reason, the sess-2 row still renames, exit 1.
        assert_eq!(code, 1);
        let registry = crate::state::load_registry(&path).unwrap();
        let by_name = |n: &str| registry.entries.iter().find(|e| e.name == n);
        assert!(
            by_name("05dddd48").is_some(),
            "the skipped row is untouched"
        );
        assert!(by_name("ghost").is_some(), "the other row still renames");
        std::fs::remove_dir_all(&dir).ok();
    }
}

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

/// Rename a registry label and, when the session holds a named live crown,
/// its crown record in the same RPC.
pub(crate) fn respond(
    home: &crate::paths::AgentsHome,
    req: &crate::protocol::Request,
) -> crate::protocol::Response {
    use crate::protocol::{ErrorCode, Response};
    let token = match req.params.get("name").and_then(Value::as_str) {
        Some(token) if !token.is_empty() => token,
        _ => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "rename needs a <name> (current label, short id, or full session id)",
            )
        }
    };
    let Some(new_name) = req.params.get("new_name").and_then(Value::as_str) else {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "rename needs --name <new-label>",
        );
    };
    if !crate::state::is_valid_registry_label(new_name) {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "registry name must be 1-64 letters, numbers, underscores, hyphens, or apostrophes",
        );
    }
    let registry_path = home.registry_json();
    let registry = match crate::state::load_registry(&registry_path) {
        Ok(registry) => registry,
        Err(error) => return Response::err(req.id, ErrorCode::Internal, error.to_string()),
    };
    let source = match crate::state::resolve_rename_source(&registry.entries, token) {
        Ok(source) => source,
        Err(error) => return Response::err(req.id, ErrorCode::Internal, error),
    };
    let old_label = source.name.clone();
    let session = source.harness_session_id.clone();
    let crown = match session.as_deref() {
        Some(session) => match crate::crown_names::rename_crown(
            &home.crown_names_json(),
            &registry_path,
            session,
            new_name,
            false,
        ) {
            Ok(crown) => crown,
            Err(error) => {
                return Response::err(req.id, ErrorCode::InvalidParams, error);
            }
        },
        None => None,
    };
    let label = if crown.is_some() {
        new_name.to_ascii_lowercase()
    } else {
        new_name.to_string()
    };
    let (old, new) = match crate::state::rename_agent(&registry_path, token, &label, None) {
        Ok(result) => result,
        Err(error) => return Response::err(req.id, ErrorCode::Internal, error),
    };
    let crown = match (crown, session.as_deref()) {
        (Some(_), Some(session)) => {
            let result = crate::crown_names::rename_crown(
                &home.crown_names_json(),
                &registry_path,
                session,
                new_name,
                true,
            )
            .and_then(|crown| {
                crown.ok_or_else(|| "the named live crown changed before rename".to_string())
            });
            match result {
                Ok(crown) => Some(crown),
                Err(crown_error) => {
                    let rollback =
                        crate::state::rename_agent(&registry_path, session, &old_label, None);
                    let rollback_result = match rollback {
                        Ok(_) => "registry label rolled back".to_string(),
                        Err(error) => format!("registry rollback failed: {error}"),
                    };
                    return Response::err(
                        req.id,
                        ErrorCode::Internal,
                        format!("crown rename failed: {crown_error}; {rollback_result}"),
                    );
                }
            }
        }
        _ => None,
    };
    Response::ok(
        req.id,
        serde_json::json!({
            "renamed": true,
            "old_name": old,
            "new_name": new,
            "crown": crown.map(|(from, to)| serde_json::json!({"from": from, "to": to})),
        }),
    )
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
    let mut receipt = format!("renamed {old} -> {new}");
    if let Some(crown) = result.get("crown") {
        if let (Some(from), Some(to)) = (
            crown.get("from").and_then(Value::as_str),
            crown.get("to").and_then(Value::as_str),
        ) {
            receipt.push_str(&format!("; crown {from} -> {to}"));
        }
    }
    Some(receipt)
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
        let matching = vec![row("vellum", "sess-2")];
        assert!(plan_from_journal(&matching, &[spawned("sess-2", "vellum", 100, 1)]).is_empty());
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

    #[test]
    fn a_crowned_codex_rename_shows_one_name_everywhere() {
        use crate::protocol::{Request, ResponsePayload};

        let dir = tmpdir("crowned-rename");
        let home = AgentsHome::at(&dir);
        let session = "01a0ee3f-235d-7671-8fbb-e09af1d5fb52";
        let other_session = "01a0ee3f-235d-7671-8fbb-e09af1d5fb53";
        crate::state::update_registry(&home.registry_json(), |registry| {
            let mut crowned = row("kestrel", session);
            crowned.harness = Some("codex".into());
            crowned.status = crate::AgentStatus::Ready;
            crowned.crown_scope = Some("x-test".into());
            crowned.crown_level = Some(2);
            let mut other = row("raven", other_session);
            other.harness = Some("codex".into());
            other.status = crate::AgentStatus::Ready;
            other.crown_scope = Some("y-test".into());
            other.crown_level = Some(2);
            registry.entries = vec![crowned, other];
        })
        .unwrap();
        std::fs::write(
            home.crown_names_json(),
            serde_json::json!({
                "version": 1,
                "crowns": {
                    "x-test": {
                        "name": "Kestrel",
                        "regnal": 2,
                        "holder_session": session,
                        "nodes": [],
                        "updated_at": "2026-09-29T00:00:00Z"
                    },
                    "y-test": {
                        "name": "Bob",
                        "regnal": 1,
                        "holder_session": other_session,
                        "nodes": [],
                        "updated_at": "2026-09-29T00:00:00Z"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let before_registry = std::fs::read(home.registry_json()).unwrap();
        let before_crowns = std::fs::read(home.crown_names_json()).unwrap();
        let duplicate = respond(
            &home,
            &Request::new(
                1,
                "agent.rename",
                serde_json::json!({"name": "kestrel", "new_name": "bob"}),
            ),
        );
        let ResponsePayload::Err(error) = duplicate.payload else {
            panic!("duplicate crown name was accepted");
        };
        assert!(error
            .message
            .contains("the name Bob is held by raven over y-test"));
        assert_eq!(
            std::fs::read(home.registry_json()).unwrap(),
            before_registry
        );
        assert_eq!(
            std::fs::read(home.crown_names_json()).unwrap(),
            before_crowns
        );
        crate::state::update_registry(&home.registry_json(), |registry| {
            registry.entries.retain(|entry| entry.name != "raven");
        })
        .unwrap();
        std::fs::write(
            home.crown_names_json(),
            serde_json::json!({
                "version": 1,
                "crowns": {
                    "x-test": {
                        "name": "Kestrel",
                        "regnal": 2,
                        "holder_session": session,
                        "nodes": [],
                        "updated_at": "2026-09-29T00:00:00Z"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let response = respond(
            &home,
            &Request::new(
                1,
                "agent.rename",
                serde_json::json!({"name": "kestrel", "new_name": "bob"}),
            ),
        );
        let ResponsePayload::Ok(result) = response.payload else {
            panic!("rename failed: {:?}", response.payload);
        };
        assert_eq!(
            receipt("kestrel", &result).as_deref(),
            Some("renamed kestrel -> bob; crown Kestrel II -> Bob")
        );

        let registry = crate::state::load_registry(&home.registry_json()).unwrap();
        let renamed = registry
            .entries
            .iter()
            .find(|entry| entry.name == "bob")
            .unwrap();
        assert!(renamed.aliases.iter().any(|alias| alias == "kestrel"));
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.crown_names_json()).unwrap()).unwrap();
        assert_eq!(record["crowns"]["x-test"]["name"], "bob");
        assert_eq!(record["crowns"]["x-test"]["regnal"], 1);
        assert_eq!(record["crowns"]["x-test"]["holder_session"], session);
        assert!(crate::crown_names::ensure_named_crown(
            &home.crown_names_json(),
            &home.registry_json(),
            "x-test"
        )
        .unwrap());
        let envelope = crate::mail_envelope::render_at(
            &serde_json::json!({
                "mode": "tag",
                "from_session": session,
                "to": "x"
            }),
            &home.registry_json(),
        )
        .unwrap();
        assert!(envelope.contains("from_name=\"bob\""));
        assert_eq!(
            crate::king_checkin::title_rename_command("codex", "bob", None),
            None
        );
        assert_eq!(
            crate::king_checkin::title_rename_command("claude", "bob", None).as_deref(),
            Some("/rename bob")
        );

        let before_registry = std::fs::read(home.registry_json()).unwrap();
        let before_crowns = std::fs::read(home.crown_names_json()).unwrap();
        let refused = respond(
            &home,
            &Request::new(
                2,
                "agent.rename",
                serde_json::json!({"name": "bob", "new_name": "bob_2"}),
            ),
        );
        assert!(matches!(refused.payload, ResponsePayload::Err(_)));
        assert_eq!(
            std::fs::read(home.registry_json()).unwrap(),
            before_registry
        );
        assert_eq!(
            std::fs::read(home.crown_names_json()).unwrap(),
            before_crowns
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

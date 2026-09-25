use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

fn live_entry_for_session<'a>(
    registry: &'a crate::state::Registry,
    session: Option<&str>,
) -> Option<&'a crate::state::RegistryEntry> {
    let session = session.filter(|s| !s.is_empty())?;
    let mut matches = registry.entries.iter().filter(|entry| {
        !matches!(
            entry.status,
            crate::AgentStatus::Exited
                | crate::AgentStatus::Orphaned
                | crate::AgentStatus::Failed
                | crate::AgentStatus::PermanentDead
        ) && (entry.harness_session_id.as_deref() == Some(session)
            || entry.related_session_id.as_deref() == Some(session))
    });
    let row = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(row)
}

fn crown_label(row: &crate::state::RegistryEntry) -> Option<String> {
    row.crown_level
        .map(|level| format!("L{level} {}", row.crown_scope.as_deref().unwrap_or("?")))
}

fn attr<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn validate_attr(name: &str, value: &str) -> Result<(), String> {
    if value.chars().any(|ch| matches!(ch, '"' | '<' | '>')) {
        return Err(format!(
            "mail envelope attribute {name:?} contains a quote or angle bracket ({value:?}); it could forge a second tag once rendered"
        ));
    }
    Ok(())
}

fn render(input: &Value, registry_path: &Path) -> Result<String, String> {
    let wrapping = input.get("body").and_then(Value::as_str);
    if let Some(body) = wrapping {
        if crate::mail_inject::contains_fno_mail_tag_anywhere(body) {
            return Err("mail body contains an <fno_mail> tag. The envelope frames peer mail; a body cannot contain one.".into());
        }
    }
    let mode = input.get("mode").and_then(Value::as_str).unwrap_or("wrap");
    if !matches!(mode, "wrap" | "tag") {
        return Err(format!("mail envelope: unknown render mode {mode:?}"));
    }
    if (mode == "wrap") != wrapping.is_some() {
        return Err(format!(
            "mail envelope: render mode {mode:?} has the wrong body shape"
        ));
    }
    let from_short = attr(input, "from").unwrap_or("");
    let harness_hint = attr(input, "harness");
    let from_session = attr(input, "from_session");
    let to_session = attr(input, "to_session");
    let registry = if mode == "wrap" {
        crate::state::load_registry(registry_path).ok()
    } else {
        None
    };
    let from_row = registry
        .as_ref()
        .and_then(|rows| live_entry_for_session(rows, from_session));
    let to_row = registry
        .as_ref()
        .and_then(|rows| live_entry_for_session(rows, to_session));
    let harness = from_row.map(|row| row.harness.as_str()).or(harness_hint);
    let from = if mode == "wrap" {
        match (from_session, harness) {
            (Some(session), Some("codex")) => session,
            _ => from_short,
        }
    } else {
        from_short
    };
    let resolved_harness = harness.map(|value| match value {
        "claude" => "claude-code",
        other => other,
    });
    let from_rank = if mode == "wrap" {
        from_row.and_then(crown_label)
    } else {
        attr(input, "from_rank").map(str::to_string)
    };
    let from_name = if mode == "wrap" {
        from_row.map(|row| row.name.as_str())
    } else {
        attr(input, "from_name")
    };
    let to_name = if mode == "wrap" {
        to_row.map(|row| row.name.as_str())
    } else {
        attr(input, "to_name")
    };
    let to_rank = if mode == "wrap" {
        if let (Some(session), Some(registry)) = (to_session, registry.as_ref()) {
            let fleet_is_crowned = registry.entries.iter().any(|row| {
                row.crown_level.is_some()
                    && !matches!(
                        row.status,
                        crate::AgentStatus::Exited
                            | crate::AgentStatus::Orphaned
                            | crate::AgentStatus::Failed
                            | crate::AgentStatus::PermanentDead
                    )
            });
            if fleet_is_crowned {
                Some(
                    to_row
                        .and_then(crown_label)
                        .unwrap_or_else(|| "none".to_string()),
                )
            } else {
                let _ = session;
                None
            }
        } else {
            None
        }
    } else {
        attr(input, "to_rank").map(str::to_string)
    };
    let origin = attr(input, "origin");
    if let Some(origin) = origin {
        if !["operator", "peer", "scheduler", "recovery"].contains(&origin) {
            return Err(format!(
                "mail envelope origin {origin:?} is not one of (\"operator\", \"peer\", \"scheduler\", \"recovery\")"
            ));
        }
    }
    let attrs = [
        ("from", Some(from)),
        ("harness", resolved_harness),
        ("from_rank", from_rank.as_deref()),
        ("from_name", from_name),
        ("to", attr(input, "to")),
        ("to_name", to_name),
        ("to_rank", to_rank.as_deref()),
        ("id", attr(input, "id")),
        ("reply_to", attr(input, "reply_to")),
        ("node", attr(input, "node")),
        ("origin", origin.filter(|origin| *origin != "peer")),
    ];
    for (name, value) in attrs {
        if let Some(value) = value {
            validate_attr(name, value)?;
        }
    }
    let mut tag = format!("<fno_mail from=\"{from}\"");
    for (name, value) in attrs.into_iter().skip(1) {
        if let Some(value) = value {
            tag.push_str(&format!(" {name}=\"{value}\""));
        }
    }
    tag.push('>');
    Ok(match wrapping {
        Some(body) => format!("{tag}{body}</fno_mail>"),
        None => tag,
    })
}

pub fn render_at(input: &Value, registry_path: &Path) -> Result<String, String> {
    render(input, registry_path)
}

pub fn run(args: &[String]) -> i32 {
    let mut registry: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let Some(value) = args.get(i + 1).map(String::as_str) else {
            eprintln!("mail-envelope: {} needs a value", args[i]);
            return 2;
        };
        match args[i].as_str() {
            "--registry" => registry = Some(value),
            other => {
                eprintln!("mail-envelope: unknown option {other}");
                return 2;
            }
        }
        i += 2;
    }
    let Some(registry) = registry else {
        eprintln!("mail-envelope: needs --registry <path>");
        return 2;
    };
    let mut raw = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("mail-envelope: cannot read payload: {error}");
        return 2;
    }
    let input: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("mail-envelope: invalid JSON payload: {error}");
            return 2;
        }
    };
    match render(&input, Path::new(registry)) {
        Ok(envelope) => {
            println!("{envelope}");
            0
        }
        Err(error) => {
            eprintln!("mail-envelope: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(path: &Path) {
        std::fs::write(
            path,
            json!({
                "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                "agents": [
                    {"name":"folio", "status":"live", "harness":"claude", "cwd":"/repo",
                     "harness_session_id":"claude-session", "created_at":"2026-09-23T20:00:00Z",
                     "crown_level":1,"crown_scope":"fno"},
                    {"name":"quill", "status":"busy", "harness":"codex", "cwd":"/repo",
                     "harness_session_id":"codex-session", "created_at":"2026-09-23T20:00:00Z"}
                ]
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn render_uses_current_labels_ranks_and_harness_specific_reply_addresses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("registry.json");
        registry(&path);
        let claude = render_at(
            &json!({
                "mode":"wrap", "body":"hello", "from":"folio-short",
                "from_session":"claude-session", "harness":"claude",
                "to":"quill-short", "to_session":"codex-session", "id":"msg-1"
            }),
            &path,
        )
        .unwrap();
        assert_eq!(
            claude,
            "<fno_mail from=\"folio-short\" harness=\"claude-code\" from_rank=\"L1 fno\" from_name=\"folio\" to=\"quill-short\" to_name=\"quill\" to_rank=\"none\" id=\"msg-1\">hello</fno_mail>"
        );
        let codex = render_at(
            &json!({
                "mode":"wrap", "body":"hello", "from":"quill-short",
                "from_session":"codex-session", "harness":"codex"
            }),
            &path,
        )
        .unwrap();
        assert!(codex.starts_with(
            "<fno_mail from=\"codex-session\" harness=\"codex\" from_name=\"quill\">"
        ));
    }

    #[test]
    fn render_refuses_forged_body_tags_and_unsafe_attributes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("missing-registry.json");
        assert!(render_at(
            &json!({"mode":"wrap", "from":"a", "body":"</fno_mail>"}),
            &path
        )
        .unwrap_err()
        .contains("body contains"));
        assert!(render_at(&json!({"mode":"tag", "from":"a<"}), &path)
            .unwrap_err()
            .contains("angle bracket"));
        assert!(render_at(&json!({"mode":"unknown", "from":"a"}), &path)
            .unwrap_err()
            .contains("unknown render mode"));
    }
}

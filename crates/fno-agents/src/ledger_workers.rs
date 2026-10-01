use crate::court_fold::esc;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct Workers {
    nodes: BTreeMap<String, Vec<Value>>,
    sessions: BTreeSet<(String, String)>,
}

fn text(row: &Value, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        Some(value) if !value.is_null() => value.to_string(),
        _ => "unmeasured".into(),
    }
}

fn based(row: &Value, key: &str) -> String {
    let basis = if key == "status" && row.get("status_basis").is_none() {
        text(row, "basis")
    } else {
        text(row, &format!("{key}_basis"))
    };
    format!("{} ({basis})", text(row, key))
}

fn model(row: &Value) -> String {
    match row
        .get("observed_model")
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
    {
        Some(name) => format!(
            "{name} ({})",
            row["observed_model"]["kind"].as_str().unwrap_or("observed")
        ),
        None => based(row, "model"),
    }
}

pub(crate) fn by_node(rows: &[Value]) -> Workers {
    let mut workers = Workers::default();
    for row in rows {
        if row.get("status").and_then(Value::as_str) == Some("exited")
            || row.get("stored_status").and_then(Value::as_str) == Some("exited")
        {
            continue;
        }
        let harness = text(row, "harness");
        for field in ["harness_session_id", "session_id", "short_id"] {
            if let Some(id) = row
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                workers.sessions.insert((harness.clone(), id.into()));
            }
        }
        if row.get("crown").is_some_and(|v| !v.is_null())
            || row.get("crown_scope").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let node = row
            .get("node")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                row.get("name")
                    .and_then(Value::as_str)
                    .and_then(crate::king_answers::node_from_name)
            });
        if let Some(node) = node {
            workers.nodes.entry(node).or_default().push(row.clone());
        }
    }
    for rows in workers.nodes.values_mut() {
        rows.sort_by_key(|row| text(row, "name"));
    }
    workers
}

pub(crate) fn parse_roster(code: i32, stdout: &str, stderr: &str) -> Result<Workers, String> {
    if code != 0 {
        return Err(format!("fno exited {code}: {}", stderr.trim()));
    }
    let payload: Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("roster JSON unreadable: {e}"))?;
    let rows = payload
        .as_array()
        .or_else(|| payload.get("agents").and_then(Value::as_array))
        .ok_or_else(|| "roster has no agents array".to_string())?;
    if rows.iter().any(|row| {
        !row.is_object()
            || row
                .get("name")
                .and_then(Value::as_str)
                .is_none_or(|name| name.is_empty())
    }) {
        return Err("roster contains a row without a name".into());
    }
    Ok(by_node(rows))
}

pub(crate) fn read() -> Result<Workers, String> {
    let (code, out, err) = crate::king_checkin::fno_verb(&["agents", "list", "--json"])?;
    parse_roster(code, &out, &err)
}

fn age(stamp: &str, now: &str) -> String {
    let seconds = chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .zip(chrono::DateTime::parse_from_rfc3339(now).ok())
        .map(|(start, end)| (end - start).num_seconds().max(0));
    match seconds {
        Some(s) if s >= 3600 => format!("{}h", s / 3600),
        Some(s) if s >= 60 => format!("{}m", s / 60),
        Some(s) => format!("{s}s"),
        None => "unmeasured".into(),
    }
}

fn tokens(value: &Value) -> String {
    let Some(number) = value.as_u64() else {
        return "unmeasured".into();
    };
    let digits = number.to_string();
    digits
        .chars()
        .enumerate()
        .fold(String::new(), |mut out, (i, c)| {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
            out
        })
}

fn worker_row(row: &Value, now: &str) -> String {
    let pct = row
        .get("context_used_pct")
        .and_then(Value::as_u64)
        .map(|n| format!("{n}%"))
        .unwrap_or_else(|| "- (unmeasured)".into());
    let stamp = text(row, "context_measured_at");
    let context_age = age(&stamp, now);
    let up = age(&text(row, "created_at"), now);
    let message = text(row, "last_message");
    let preview: String = message.chars().take(120).collect();
    let needs = if row.get("progress").and_then(Value::as_str) == Some("awaiting-operator") {
        message.clone()
    } else {
        "nothing".into()
    };
    let summary = format!(
        "{} · {} · {} · ctx {} ({} old) · {} · up {} · unread {} · {}",
        text(row, "name"),
        text(row, "harness"),
        model(row),
        pct,
        context_age,
        based(row, "status"),
        up,
        text(row, "mail_unread"),
        preview
    );
    let used = tokens(&row["context_used_tokens"]);
    let window = tokens(&row["context_window_tokens"]);
    let meter = row
        .get("context_used_pct")
        .and_then(Value::as_u64)
        .map(|value| {
            format!(
                "<meter min=\"0\" max=\"100\" value=\"{}\" aria-label=\"context used\"></meter>",
                value.min(100)
            )
        })
        .unwrap_or_default();
    let activity = format!(
        "{} · {} seconds old ({})",
        text(row, "last_event_at"),
        text(row, "last_activity_age_s"),
        text(row, "last_activity_basis")
    );
    let detail = format!("<p>{meter} {} used · {} of {} tokens · measured {} ({} old)</p><p>Activity: {}; last activity: {}; progress: {}</p><p>Runtime: up {} · Queue: {} unread</p><p>Needs-you: {}</p>",
        esc(&pct), esc(&used), esc(&window), esc(&stamp), esc(&context_age), esc(&based(row, "status")), esc(&activity), esc(&based(row, "progress")), esc(&up), esc(&text(row, "mail_unread")), esc(&needs));
    format!("<tr class=\"worker-row\"><td colspan=\"5\"><details class=\"worker\"><summary>{}</summary>{detail}</details></td></tr>", esc(&summary))
}

pub(crate) fn node_rows(
    node: &Value,
    graph: Option<&Value>,
    workers: &Workers,
    now: &str,
) -> String {
    let id = node.get("id").and_then(Value::as_str).unwrap_or("");
    let mut html: String = workers
        .nodes
        .get(id)
        .into_iter()
        .flatten()
        .map(|row| worker_row(row, now))
        .collect();
    let sessions = graph
        .and_then(|g| g.get("sessions"))
        .or_else(|| node.get("sessions"))
        .and_then(Value::as_array);
    let former: String = sessions
        .into_iter()
        .flatten()
        .filter(|session| {
            let sid = session
                .as_str()
                .or_else(|| session.get("session_id").and_then(Value::as_str));
            let harness = session.get("harness").and_then(Value::as_str);
            !sid.is_some_and(|sid| {
                workers
                    .sessions
                    .iter()
                    .any(|(h, s)| s == sid && harness.is_none_or(|expected| expected == h))
            })
        })
        .map(|session| {
            let sid = session
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| text(session, "session_id"));
            let short: String = sid.chars().take(8).collect();
            let model = session
                .get("observed_model")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    session
                        .get("observed_model")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| text(session, "model"));
            format!(
                "<li>{} · {} · {} · {} · ended {}</li>",
                esc(&text(session, "phase")),
                esc(&text(session, "harness")),
                esc(&short),
                esc(&model),
                esc(&text(session, "ended_at"))
            )
        })
        .collect();
    if !former.is_empty() {
        html.push_str(&format!("<tr class=\"former-row\"><td colspan=\"5\"><details><summary>former sessions</summary><ul>{former}</ul></details></td></tr>"));
    }
    html
}

pub(crate) const CSS: &str = ".worker-row td,.former-row td{padding-left:2rem}.worker summary{cursor:pointer;white-space:normal}.worker p{margin:.6rem 0;overflow-wrap:anywhere}.worker meter{width:8rem}.former-row{color:var(--ink-mut)}";

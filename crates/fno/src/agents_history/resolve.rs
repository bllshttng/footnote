use super::{eq, event_sid, event_time, push_string_unique, push_unique, str_at, Receipt, Sources};
use serde_json::Value;
use std::collections::HashMap;

pub(super) struct Resolved {
    pub(super) sessions: Vec<String>,
    pub(super) ledger_only: Vec<Value>,
    pub(super) repo_slug_unresolved: bool,
}
pub(super) fn resolve(arg: &str, sources: &Sources) -> Resolved {
    let mut found: HashMap<String, (String, String)> = HashMap::new();
    let mut ledger_only = Vec::new();
    let mut repo_slug_unresolved = false;
    let handle = arg.trim();

    if let Some(pr) = parse_pr(handle) {
        repo_slug_unresolved = sources.repo_slug.is_none();
        if let Ok(graph) = &sources.graph {
            for node in graph.iter().filter(|node| pr_matches(node, pr)) {
                if !slug_matches(node, sources.repo_slug.as_deref()) {
                    continue;
                }
                add_graph_sessions(&mut found, node);
            }
        }
        if let Ok(ledger) = &sources.ledger {
            for entry in ledger.iter().filter(|entry| pr_matches(entry, pr)) {
                if !slug_matches(entry, sources.repo_slug.as_deref()) {
                    continue;
                }
                add_entry_sessions(&mut found, entry);
                if session_ids(entry).is_empty() {
                    push_unique(&mut ledger_only, entry.clone());
                }
            }
        }
    } else if let Some(node_id) = parse_node(handle).filter(|id| node_reference_exists(id, sources))
    {
        if let Ok(graph) = &sources.graph {
            let exact = graph
                .iter()
                .filter(|node| node_id_of(node).is_some_and(|id| eq(id, &node_id)))
                .collect::<Vec<_>>();
            let matches = if exact.is_empty() {
                graph
                    .iter()
                    .filter(|node| node_id_of(node).is_some_and(|id| node_id_matches(id, &node_id)))
                    .collect()
            } else {
                exact
            };
            for node in matches {
                add_graph_sessions(&mut found, node);
            }
        }
        if let Ok(ledger) = &sources.ledger {
            let exact = ledger
                .iter()
                .filter(|entry| entry_node_matches_exact(entry, &node_id))
                .collect::<Vec<_>>();
            let matches = if exact.is_empty() {
                ledger
                    .iter()
                    .filter(|entry| entry_node_matches(entry, &node_id))
                    .collect()
            } else {
                exact
            };
            for entry in matches {
                add_entry_sessions(&mut found, entry);
                if session_ids(entry).is_empty() {
                    push_unique(&mut ledger_only, entry.clone());
                }
            }
        }
        if let Ok(receipts) = &sources.receipts {
            let exact = receipts
                .iter()
                .filter(|receipt| receipt_node_matches_exact(receipt, &node_id))
                .collect::<Vec<_>>();
            let matches = if exact.is_empty() {
                receipts
                    .iter()
                    .filter(|receipt| receipt_node_matches(receipt, &node_id))
                    .collect()
            } else {
                exact
            };
            for receipt in matches {
                if let Some(sid) = receipt.value["harness_session_id"].as_str() {
                    record_session(&mut found, sid, str_at(&receipt.value, "reaped_at"));
                }
            }
        }
    } else if !handle.is_empty() {
        let live_name_match = sources.registry.as_ref().ok().is_some_and(|rows| {
            rows.iter()
                .any(|row| row_is_live(row) && registry_name_matches(row, handle))
        });
        if let Ok(rows) = &sources.registry {
            for row in rows
                .iter()
                .filter(|row| registry_matches_handle(row, handle))
            {
                if let Some(sid) = str_at(row, "harness_session_id") {
                    record_session(&mut found, sid, latest_registry_time(row));
                }
            }
        }
        if let Ok(receipts) = &sources.receipts {
            for receipt in receipts.iter() {
                if live_name_match && receipt_name_matches(receipt, handle) {
                    continue;
                }
                if receipt_matches_handle(receipt, handle) {
                    if let Some(sid) = str_at(&receipt.value, "harness_session_id") {
                        let live = sources.registry.as_ref().ok().is_some_and(|rows| {
                            rows.iter().any(|row| {
                                row_is_live(row)
                                    && str_at(row, "harness_session_id").is_some_and(|v| eq(v, sid))
                            })
                        });
                        if !live {
                            record_session(&mut found, sid, str_at(&receipt.value, "reaped_at"));
                        }
                    }
                }
            }
        }
        if let Ok(graph) = &sources.graph {
            for node in graph {
                for session in graph_session_rows(node) {
                    if str_at(session, "session_id")
                        .is_some_and(|sid| session_handle_matches(sid, handle))
                    {
                        record_session(
                            &mut found,
                            str_at(session, "session_id").unwrap_or_default(),
                            graph_session_time(session),
                        );
                    }
                }
            }
        }
        if let Ok(ledger) = &sources.ledger {
            for entry in ledger {
                if session_ids(entry)
                    .iter()
                    .any(|sid| session_handle_matches(sid, handle))
                {
                    add_entry_sessions(&mut found, entry);
                }
            }
        }
        if let Ok(events) = &sources.events {
            for event in events {
                if let Some(sid) =
                    event_sid(event).filter(|sid| session_handle_matches(sid, handle))
                {
                    record_session(&mut found, sid, event_time(event));
                }
            }
        }
    }

    let mut sessions: Vec<(String, String)> =
        found.into_values().map(|(sid, time)| (sid, time)).collect();
    sessions.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });
    Resolved {
        sessions: sessions.into_iter().map(|(sid, _)| sid).collect(),
        ledger_only,
        repo_slug_unresolved,
    }
}

fn record_session(
    found: &mut HashMap<String, (String, String)>,
    sid: &str,
    timestamp: Option<&str>,
) {
    if sid.trim().is_empty() || sid.starts_with("unresolved:") {
        return;
    }
    let key = sid.to_ascii_lowercase();
    let stamp = timestamp.unwrap_or_default().to_string();
    match found.get_mut(&key) {
        Some((_, previous)) if stamp > *previous => *previous = stamp,
        Some(_) => {}
        None => {
            found.insert(key, (sid.to_string(), stamp));
        }
    }
}

fn add_graph_sessions(found: &mut HashMap<String, (String, String)>, node: &Value) {
    for session in graph_session_rows(node) {
        if let Some(sid) = str_at(session, "session_id") {
            record_session(found, sid, graph_session_time(session));
        }
    }
}

fn add_entry_sessions(found: &mut HashMap<String, (String, String)>, entry: &Value) {
    for sid in session_ids(entry) {
        record_session(found, &sid, ledger_time(entry));
    }
}

fn session_ids(entry: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for key in ["session_id", "fno_id"] {
        if let Some(sid) = str_at(entry, key).filter(|sid| !sid.starts_with("unresolved:")) {
            push_string_unique(&mut ids, sid.to_string());
        }
    }
    if let Some(values) = entry.get("sessions").and_then(Value::as_array) {
        for value in values {
            let sid = value.as_str().or_else(|| str_at(value, "session_id"));
            if let Some(sid) = sid.filter(|sid| !sid.starts_with("unresolved:")) {
                push_string_unique(&mut ids, sid.to_string());
            }
        }
    }
    ids
}

fn graph_session_rows(node: &Value) -> Vec<&Value> {
    node.get("sessions")
        .and_then(Value::as_array)
        .map(|sessions| sessions.iter().collect())
        .unwrap_or_default()
}

fn parse_node(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    let normalized = value.to_ascii_lowercase();
    let wellformed = if let Some((prefix, suffix)) = normalized.split_once('-') {
        !suffix.contains('-') && wellformed_node_parts(prefix, suffix)
    } else {
        (1..normalized.len())
            .any(|split| wellformed_node_parts(&normalized[..split], &normalized[split..]))
    };
    wellformed.then_some(normalized)
}

fn wellformed_node_parts(prefix: &str, suffix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 8
        && prefix.as_bytes()[0].is_ascii_lowercase()
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && (4..=8).contains(&suffix.len())
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn node_id_matches(stored: &str, query: &str) -> bool {
    if eq(stored, query) {
        return true;
    }
    let (Some(stored), Some(query)) = (parse_node(stored), parse_node(query)) else {
        return false;
    };
    if stored.contains('-') == query.contains('-') {
        return false;
    }
    stored.replace('-', "") == query.replace('-', "")
}

fn node_reference_exists(query: &str, sources: &Sources) -> bool {
    sources.graph.as_ref().ok().is_some_and(|rows| {
        rows.iter()
            .any(|row| node_id_of(row).is_some_and(|id| node_id_matches(id, query)))
    }) || sources
        .ledger
        .as_ref()
        .ok()
        .is_some_and(|rows| rows.iter().any(|row| entry_node_matches(row, query)))
        || sources
            .receipts
            .as_ref()
            .ok()
            .is_some_and(|rows| rows.iter().any(|row| receipt_node_matches(row, query)))
}

pub(super) fn parse_pr(value: &str) -> Option<u64> {
    let value = value.trim();
    let number = if let Some(url_at) = value.to_ascii_lowercase().find("github.com/") {
        let path = &value[url_at + "github.com/".len()..];
        let parts: Vec<&str> = path
            .split(['?', '#'])
            .next()?
            .trim_end_matches('/')
            .split('/')
            .collect();
        let pull = parts
            .iter()
            .position(|part| part.eq_ignore_ascii_case("pull"))?;
        *parts.get(pull + 1)?
    } else {
        value.strip_prefix('#').unwrap_or(value)
    };
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
        .then(|| number.parse().ok())?
}

fn pr_matches(row: &Value, pr: u64) -> bool {
    row.get("pr_number")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .or_else(|| str_at(row, "pr_url").and_then(parse_pr))
        == Some(pr)
}

fn slug_matches(row: &Value, requested: Option<&str>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    let Some(url) = str_at(row, "pr_url") else {
        return false;
    };
    pr_slug(url).is_some_and(|slug| normalize_slug(&slug) == normalize_slug(requested))
}

fn pr_slug(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let at = lower.find("github.com/")? + "github.com/".len();
    let path = url[at..].split(['?', '#']).next()?.trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || !parts[2].eq_ignore_ascii_case("pull") {
        return None;
    }
    Some(format!("{}/{}", parts[0], parts[1]))
}

fn normalize_slug(slug: &str) -> String {
    slug.trim()
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(slug.trim().trim_end_matches('/'))
        .to_ascii_lowercase()
}

fn entry_node_matches(entry: &Value, id: &str) -> bool {
    entry_node_matches_exact(entry, id)
        || ["graph_node_id", "node"]
            .iter()
            .any(|key| str_at(entry, key).is_some_and(|v| node_id_matches(v, id)))
}

fn entry_node_matches_exact(entry: &Value, id: &str) -> bool {
    ["graph_node_id", "node"]
        .iter()
        .any(|key| str_at(entry, key).is_some_and(|v| eq(v, id)))
}

fn receipt_node_matches(receipt: &Receipt, id: &str) -> bool {
    receipt_node_matches_exact(receipt, id)
        || ["graph_node_id", "node"].iter().any(|key| {
            str_at(&receipt.value["ledger"], key).is_some_and(|v| node_id_matches(v, id))
        })
}

fn receipt_node_matches_exact(receipt: &Receipt, id: &str) -> bool {
    ["graph_node_id", "node"]
        .iter()
        .any(|key| str_at(&receipt.value["ledger"], key).is_some_and(|v| eq(v, id)))
}

pub(super) fn node_id_of(node: &Value) -> Option<&str> {
    str_at(node, "id").or_else(|| str_at(node, "graph_node_id"))
}

fn registry_matches_handle(row: &Value, handle: &str) -> bool {
    ["harness_session_id", "short_id", "fno_id"]
        .iter()
        .any(|key| str_at(row, key).is_some_and(|value| session_handle_matches(value, handle)))
        || registry_name_matches(row, handle)
}

fn registry_name_matches(row: &Value, handle: &str) -> bool {
    str_at(row, "name").is_some_and(|name| eq(name, handle))
        || row
            .get("aliases")
            .and_then(Value::as_array)
            .is_some_and(|aliases| {
                aliases
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|alias| eq(alias, handle))
            })
}

fn receipt_matches_handle(receipt: &Receipt, handle: &str) -> bool {
    ["harness_session_id", "short_id"].iter().any(|key| {
        str_at(&receipt.value, key).is_some_and(|value| session_handle_matches(value, handle))
    }) || receipt_name_matches(receipt, handle)
}

fn receipt_name_matches(receipt: &Receipt, handle: &str) -> bool {
    str_at(&receipt.value, "row_name").is_some_and(|name| eq(name, handle))
}

fn session_handle_matches(session: &str, handle: &str) -> bool {
    if session.is_empty() || handle.is_empty() {
        return false;
    }
    let session = session.to_ascii_lowercase();
    let handle = handle.to_ascii_lowercase();
    session == handle || (handle.len() >= 8 && session.starts_with(&handle))
}

pub(super) fn row_is_live(row: &Value) -> bool {
    !matches!(
        str_at(row, "status")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "exited" | "removed" | "reaped" | "dead" | "permanent-dead"
    )
}

fn latest_registry_time(row: &Value) -> Option<&str> {
    ["exited_at", "created_at", "updated_at"]
        .iter()
        .find_map(|key| str_at(row, key))
}

fn graph_session_time(row: &Value) -> Option<&str> {
    ["ended_at", "started_at", "claimed_at"]
        .iter()
        .find_map(|key| str_at(row, key))
}

fn ledger_time(row: &Value) -> Option<&str> {
    ["completed", "completed_at", "updated_at"]
        .iter()
        .find_map(|key| str_at(row, key))
}

pub(super) fn resolve_registry<'a>(sources: &'a Sources, sid: &str) -> Option<&'a Value> {
    sources
        .registry
        .as_ref()
        .ok()?
        .iter()
        .find(|row| str_at(row, "harness_session_id").is_some_and(|value| eq(value, sid)))
}

pub(super) fn receipts_for<'a>(sources: &'a Sources, sid: &str) -> Vec<&'a Receipt> {
    let live = sources.registry.as_ref().ok().is_some_and(|rows| {
        rows.iter().any(|row| {
            row_is_live(row)
                && str_at(row, "harness_session_id").is_some_and(|value| eq(value, sid))
        })
    });
    if live {
        return Vec::new();
    }
    let mut receipts: Vec<&Receipt> = sources
        .receipts
        .as_ref()
        .ok()
        .into_iter()
        .flatten()
        .filter(|receipt| {
            str_at(&receipt.value, "harness_session_id").is_some_and(|value| eq(value, sid))
        })
        .collect();
    receipts.sort_by(|a, b| str_at(&b.value, "reaped_at").cmp(&str_at(&a.value, "reaped_at")));
    receipts
}
pub(super) fn node_for_session<'a>(
    sid: &str,
    sources: &'a Sources,
) -> Option<(&'a Value, &'a Value)> {
    let graph = sources.graph.as_ref().ok()?;
    graph.iter().find_map(|node| {
        graph_session_rows(node)
            .into_iter()
            .find(|session| str_at(session, "session_id").is_some_and(|value| eq(value, sid)))
            .map(|session| (node, session))
    })
}

pub(super) fn graph_session_rows_for<'a>(sid: &str, sources: &'a Sources) -> Vec<&'a Value> {
    sources
        .graph
        .as_ref()
        .ok()
        .into_iter()
        .flatten()
        .flat_map(graph_session_rows)
        .filter(|row| str_at(row, "session_id").is_some_and(|value| eq(value, sid)))
        .collect()
}
pub(super) fn ledger_for_session<'a>(sid: &str, sources: &'a Sources) -> Vec<&'a Value> {
    sources
        .ledger
        .as_ref()
        .ok()
        .into_iter()
        .flatten()
        .filter(|entry| session_ids(entry).iter().any(|value| eq(value, sid)))
        .collect()
}

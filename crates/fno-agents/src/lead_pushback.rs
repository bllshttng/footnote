//! The per-lead pushback reading: from the decision_span rows the x-98cf
//! envelope already journals (ask, route, correction spans), how many asks
//! reached this lead, how many routes it answered itself versus escalated,
//! and how many of its route spans a later correction overturned. The score
//! rides the check-in beside the refusal rate: the useful lead metric is
//! worker pushback where the lead turned out wrong.

use serde_json::{json, Value};

/// The lead's own pushback reading from the project events journal. Errors
/// when no session identity resolves (attribution needs one) or the journal
/// cannot be read; an empty trace history reads zeros.
pub(crate) fn reading() -> Result<Value, String> {
    let (session, _) = crate::claims::resolve_identity();
    let Some(session) = session else {
        return Err("no session id; cannot attribute pushback spans".into());
    };
    let me = crate::identity::canonical_handle(&session);
    let journal = crate::law_match::project_events_journal();
    let rows = crate::event_store::query_events(
        &journal,
        &crate::event_store::EventQuery::of_types(&["decision_span"]),
    )?;
    Ok(fold(&rows, &me))
}

/// The count fold over journal rows for one canonical lead handle: pure, so
/// tests build journal rows by hand instead of touching env or a store.
fn fold(rows: &[crate::event_store::EventRow], me: &str) -> Value {
    let mut asks = 0u64;
    let mut routes = 0u64;
    let mut answered_self = 0u64;
    let mut escalated = 0u64;
    let mut overturned = 0u64;
    let mut route_ids: Vec<String> = Vec::new();
    for row in rows {
        let Some(data) = parse_span(&row.line) else {
            continue;
        };
        match span_kind(&data).unwrap_or("") {
            "ask" => {
                if is_ask_for_me(&data, &me) {
                    asks += 1;
                }
            }
            "route" => {
                if actor_is_me(&data, &me) {
                    routes += 1;
                    route_ids.push(span_id(&data));
                    match route(&data).unwrap_or("") {
                        "self" => answered_self += 1,
                        "escalate" => escalated += 1,
                        _ => {}
                    }
                }
            }
            "correction" => {
                if overturns_one_of(&data, &route_ids) {
                    overturned += 1
                }
            }
            _ => {}
        }
    }
    let score = (routes > 0).then(|| overturned as f64 / routes as f64);
    json!({
        "session": me,
        "asks": asks,
        "routes": routes,
        "answered_self": answered_self,
        "escalated": escalated,
        "overturned": overturned,
        "score": score,
    })
}

/// One journal line's decision_span data object, None for every other row.
fn parse_span(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("decision_span") {
        return None;
    }
    value.get("data").cloned()
}

fn span_kind(data: &Value) -> Option<&str> {
    data.get("span_kind").and_then(Value::as_str)
}

fn actor_is_me(data: &Value, me: &str) -> bool {
    data.pointer("/trace/actor_session")
        .and_then(Value::as_str)
        .is_some_and(|actor| crate::identity::canonical_handle(actor) == me)
}

/// An ask span names this lead when the trace names this session as the
/// recipient, or names no session but recipient_kind lead.
fn is_ask_for_me(data: &Value, me: &str) -> bool {
    recipient_is_me(data, me)
}

fn recipient_is_me(data: &Value, me: &str) -> bool {
    data.pointer("/trace/recipient_session")
        .and_then(Value::as_str)
        .is_some_and(|recipient| crate::identity::canonical_handle(recipient) == me)
}

fn route(data: &Value) -> Option<&str> {
    data.get("route").and_then(Value::as_str)
}

fn span_id(data: &Value) -> String {
    data.pointer("/trace/span_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn overturns_one_of(data: &Value, route_ids: &[String]) -> bool {
    data.get("overturns")
        .and_then(Value::as_str)
        .is_some_and(|id| route_ids.contains(&id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One decision_span journal row for the fold.
    fn row(span_kind: &str, trace: Value, extra: Value) -> crate::event_store::EventRow {
        let mut data = extra;
        if let Value::Object(map) = &mut data {
            map.insert("span_kind".into(), json!(span_kind));
            map.insert("trace".into(), trace);
        }
        crate::event_store::EventRow {
            seq: 0,
            event_id: String::new(),
            ts_ms: 0,
            r#type: "decision_span".into(),
            source: "target".into(),
            scope: None,
            retention_class: "durable".into(),
            reject_reason: None,
            line: json!({
                "type": "decision_span",
                "source": "target",
                "data": data,
            })
            .to_string(),
            history_only: false,
            recovery_batch: None,
        }
    }

    /// The lead trace envelope the fixtures share.
    fn trace(span_id: &str) -> Value {
        json!({
            "trace_id": "x-1",
            "span_id": span_id,
            "actor_session": "sess-lead",
            "actor_kind": "lead",
            "comms": "mail",
        })
    }

    #[test]
    fn spans_join_into_the_pushback_score() {
        let rows = vec![
            row(
                "ask",
                json!({
                    "trace_id": "x-1",
                    "span_id": "s-a",
                    "actor_session": "sess-w1",
                    "actor_kind": "worker",
                    "comms": "mail",
                    "recipient_session": "sess-lead",
                    "recipient_kind": "lead",
                }),
                json!({}),
            ),
            row("route", trace("s-r1"), json!({"route": "self"})),
            row("route", trace("s-r2"), json!({"route": "escalate"})),
            row(
                "correction",
                json!({
                    "trace_id": "x-1",
                    "span_id": "s-c1",
                    "actor_session": "sess-lead",
                    "actor_kind": "user",
                    "comms": "chat",
                    "overturns": "s-r1",
                }),
                json!({}),
            ),
        ];
        let folded = fold(&rows, "sess-lead");
        assert_eq!(folded["asks"], 1);
        assert_eq!(folded["routes"], 2);
        assert_eq!(folded["answered_self"], 1);
        assert_eq!(folded["escalated"], 1);
        assert_eq!(folded["overturned"], 1);
        assert!((folded["score"].as_f64().unwrap() - 0.5).abs() < 1e-9);

        // An empty trace history reads zeros and a null score, never a
        // fabricated rate.
        let empty = fold(&[], "sess-lead");
        assert_eq!(empty["asks"], 0);
        assert_eq!(empty["routes"], 0);
        assert_eq!(empty["score"], Value::Null);
    }
}

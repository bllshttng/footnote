use serde_json::{json, Map, Value};

use crate::lead_checkin::Reading;

pub(super) fn read() -> Result<Value, String> {
    crate::lead_answers::overdue_watches_reading()
}

pub(super) fn add_data(readings: &[Reading], data: &mut Map<String, Value>) {
    if let Some(watches) = readings
        .iter()
        .find(|reading| reading.name == "watch_expiry" && reading.ok)
    {
        data.insert(
            "overdue_watches".into(),
            watches.value.get("rows").cloned().unwrap_or(json!([])),
        );
    }
}

pub(super) fn read_error(readings: &[Reading]) -> Option<&str> {
    readings
        .iter()
        .find(|reading| reading.name == "watch_expiry" && !reading.ok)
        .map(|reading| reading.error.as_str())
}

pub(super) fn workers_suffix(readings: &[Reading], data: &Map<String, Value>) -> String {
    if read_error(readings).is_some() {
        return ", overdue watches unmeasured".into();
    }
    let Some(rows) = data.get("overdue_watches").and_then(Value::as_array) else {
        return String::new();
    };
    let overdue = rows
        .iter()
        .filter_map(|row| {
            let session = row.get("session_id").and_then(Value::as_str)?;
            let millis = row.get("overdue_ms").and_then(Value::as_i64)?;
            Some(format!(
                "overdue {session} {}s",
                millis.saturating_add(999) / 1000
            ))
        })
        .collect::<Vec<_>>();
    if overdue.is_empty() {
        String::new()
    } else {
        format!(", {}", overdue.join(", "))
    }
}

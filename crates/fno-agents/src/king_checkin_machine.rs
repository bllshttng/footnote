//! The king check-in's machine reading: the newest machine_sample row,
//! folded to one line every beat prints.

use serde_json::{json, Value};
use std::path::PathBuf;

/// The newest machine_sample row across the check-in's event journals.
/// Absent or stale-beyond-an-hour reads `unmeasured`, never an old number
/// dressed as current.
pub(crate) fn newest_reading(events_paths: &[PathBuf]) -> Result<Value, String> {
    let mut newest: Option<(String, Value)> = None;
    for path in events_paths {
        if let Some((id, data)) = crate::machine_sample::newest(path) {
            let ts = data.get("_ts").and_then(Value::as_str).unwrap_or("");
            let best = newest
                .as_ref()
                .map(|(_, d)| d.get("_ts").and_then(Value::as_str).unwrap_or(""))
                .unwrap_or("");
            if newest.is_none() || ts > best {
                newest = Some((id, data));
            }
        }
    }
    let Some((_, data)) = newest else {
        return Ok(json!({"state": "unmeasured"}));
    };
    let age = data
        .get("_ts")
        .and_then(Value::as_str)
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|then| (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(i64::MAX);
    if age > 3600 {
        return Ok(json!({"state": "unmeasured", "age_s": age}));
    }
    Ok(json!({
        "state": "measured",
        "processes": data.get("processes"),
        "swap_used_gb": data.get("swap_used_gb"),
        "swap_total_gb": data.get("swap_total_gb"),
        "age_s": age,
    }))
}

/// The one render line for a machine reading.
pub(crate) fn beat_line(reading: &Value) -> String {
    let state = reading
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unmeasured");
    if state != "measured" {
        return "machine: unmeasured".into();
    }
    let num = |key: &str| {
        reading
            .get(key)
            .and_then(Value::as_f64)
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "unmeasured".into())
    };
    let count = reading
        .get("processes")
        .and_then(Value::as_u64)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unmeasured".into());
    let age = reading.get("age_s").and_then(Value::as_i64).unwrap_or(0);
    format!(
        "machine: {count} processes, swap {} of {} GB ({age}s old)",
        num("swap_used_gb"),
        num("swap_total_gb"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_reading_folds_the_newest_sample_from_the_journals() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::hours(2)).to_rfc3339();
        let new = now.to_rfc3339();
        let rows = [
            serde_json::json!({"ts": old, "type": "machine_sample", "source": "daemon",
                "data": {"processes": 1005, "swap_used_gb": 2.0, "swap_total_gb": 95.0}}),
            serde_json::json!({"ts": new, "type": "machine_sample", "source": "daemon",
                "data": {"processes": 144, "swap_used_gb": 15.7, "swap_total_gb": 95.0}}),
        ];
        let body: String = rows.iter().map(|r| r.to_string() + "\n").collect();
        std::fs::write(&journal, body).unwrap();
        let reading = newest_reading(&[journal]).unwrap();
        assert_eq!(reading["state"], "measured");
        assert_eq!(reading["processes"], 144);
        assert_eq!(reading["swap_used_gb"], 15.7);
        assert!(reading["age_s"].as_i64().unwrap() >= 0);
    }

    #[test]
    fn machine_reading_reports_unmeasured_when_no_row_exists() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("absent.jsonl");
        let reading = newest_reading(&[journal]).unwrap();
        assert_eq!(reading["state"], "unmeasured");
    }

    #[test]
    fn a_stale_sample_reads_unmeasured_not_old_news() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        let stale = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let row = serde_json::json!({"ts": stale, "type": "machine_sample", "source": "daemon",
            "data": {"processes": 144, "swap_used_gb": 2.0, "swap_total_gb": 95.0}});
        std::fs::write(&journal, row.to_string() + "\n").unwrap();
        let reading = newest_reading(&[journal]).unwrap();
        assert_eq!(reading["state"], "unmeasured");
    }

    #[test]
    fn the_machine_line_renders_one_screen_line() {
        let reading = json!({
            "state": "measured",
            "processes": 144,
            "swap_used_gb": 15.7,
            "swap_total_gb": 95.0,
            "age_s": 42,
        });
        assert_eq!(
            beat_line(&reading),
            "machine: 144 processes, swap 15.7 of 95.0 GB (42s old)"
        );
    }
}

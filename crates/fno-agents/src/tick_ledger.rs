//! One tick row per control-plane arm run, and the readout built from them.
//!
//! Every scheduled arm of the control plane (king wake, watchdog, pr-watch
//! merge dispatch, active backlog, auto-continue, the stop-hook shim) appends
//! one `control_plane_tick` row to the journal it already uses, saying what it
//! did or why it did nothing. The reader folds every journal into one row per
//! arm: last tick, last action, last skip reason, and a stale verdict when the
//! last tick is older than twice the arm's interval. An arm that never ticked
//! is stale too - absence is the loudest skip reason of all.
//!
//! The row shape is owned here; the Python arms mirror it through
//! `cli/src/fno/control_plane.py` and `cli/src/fno/events/schema.yaml`, and a
//! fixture row in `cli/tests/events/parity_corpus.jsonl` keeps both
//! validators agreeing on it.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::loop_runtime::Journal;

/// The journal event type every arm writes once per run.
pub const EVENT_TYPE: &str = "control_plane_tick";

/// One known arm and the default interval its staleness is judged against.
/// The row's own `interval_s` field overrides the default, so an operator who
/// tunes an arm's config keeps the verdict honest without touching this table.
/// `0` marks an event-driven arm (the stop-hook shim): it ticks only when a
/// session stops, so staleness does not apply and it never reads red from
/// quiet.
pub struct ArmSpec {
    pub arm: &'static str,
    pub default_interval_s: u64,
}

/// Every arm the readout shows, whether or not it has ever ticked.
pub const KNOWN_ARMS: &[ArmSpec] = &[
    ArmSpec {
        arm: "king_wake",
        default_interval_s: 900,
    },
    ArmSpec {
        arm: "watchdog",
        default_interval_s: 600,
    },
    ArmSpec {
        arm: "pr_watch_merge",
        default_interval_s: 600,
    },
    ArmSpec {
        arm: "active_backlog",
        default_interval_s: 300,
    },
    ArmSpec {
        arm: "auto_continue",
        default_interval_s: 1800,
    },
    ArmSpec {
        arm: "notify_watch",
        default_interval_s: 300,
    },
    ArmSpec {
        arm: "stop_hook",
        default_interval_s: 0,
    },
];

/// Build the `data` object of one tick row. `skip_reason` is a single token
/// (`no_crowned_target`, `watchdog_off`, `env_broken`, ...); `detail` is a
/// short human string.
pub fn tick_data(
    arm: &str,
    scheduler: &str,
    acted: u64,
    skip_reason: Option<&str>,
    detail: Option<&str>,
    interval_s: u64,
) -> Value {
    let mut data = json!({
        "arm": arm,
        "scheduler": scheduler,
        "acted": acted,
        "interval_s": interval_s,
    });
    let obj = data.as_object_mut().expect("literal is an object");
    match skip_reason {
        Some(reason) => obj.insert("skip_reason".into(), Value::String(reason.to_string())),
        None => obj.insert("skip_reason".into(), Value::Null),
    };
    match detail {
        Some(text) => obj.insert("detail".into(), Value::String(text.to_string())),
        None => obj.insert("detail".into(), Value::Null),
    };
    data
}

/// Append one tick row through the loop journal (project journal + global
/// mirror), the journal the daemon arms already use.
pub fn emit_tick(
    journal: &Journal,
    arm: &str,
    scheduler: &str,
    acted: u64,
    skip_reason: Option<&str>,
    detail: Option<&str>,
    interval_s: u64,
) {
    let _ = journal.append(
        EVENT_TYPE,
        tick_data(arm, scheduler, acted, skip_reason, detail, interval_s),
    );
}

/// One rendered arm row for the readout.
#[derive(Debug, Serialize)]
pub struct ArmStatus {
    pub arm: String,
    pub scheduler: Option<String>,
    /// RFC3339 ts of the newest tick, or None when the arm never ticked.
    pub last_ts: Option<String>,
    pub age_s: Option<u64>,
    pub acted: Option<u64>,
    pub skip_reason: Option<String>,
    pub detail: Option<String>,
    pub interval_s: u64,
    /// True when the arm never ticked, or its newest tick is older than twice
    /// its interval. Event-driven arms (interval 0) never read stale.
    pub stale: bool,
}

/// Fold every journal (plus `.1` rotations) into one row per known arm.
/// Unknown arms seen in the journals are appended after the known ones, so a
/// new emitter deploys before its reader does.
pub fn read_arms(journals: &[PathBuf], now_unix: u64) -> Vec<ArmStatus> {
    let mut newest: HashMap<String, NewestTick> = HashMap::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for journal in journals {
        paths.push(journal.clone());
        paths.push(rotation_path(journal));
    }
    for path in &paths {
        scan_journal(path, &mut newest);
    }

    let mut rows: Vec<ArmStatus> = KNOWN_ARMS
        .iter()
        .map(|spec| {
            arm_status(
                spec.arm,
                spec.default_interval_s,
                newest.get(spec.arm),
                now_unix,
            )
        })
        .collect();
    let mut extra: Vec<ArmStatus> = newest
        .keys()
        .filter(|arm| !KNOWN_ARMS.iter().any(|spec| spec.arm == *arm))
        .map(|arm| arm_status(arm, 0, newest.get(arm), now_unix))
        .collect();
    extra.sort_by(|a, b| a.arm.cmp(&b.arm));
    rows.extend(extra);
    rows
}

/// The newest tick row seen for an arm: its parsed ts, the raw ts string, and
/// its data object.
struct NewestTick {
    ts_unix: u64,
    ts: String,
    data: Value,
}

fn rotation_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

fn scan_journal(path: &Path, newest: &mut HashMap<String, NewestTick>) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some(EVENT_TYPE) {
            continue;
        }
        let Some(data) = value.get("data").and_then(Value::as_object) else {
            continue;
        };
        let Some(arm) = data.get("arm").and_then(Value::as_str) else {
            continue;
        };
        let Some(ts_unix) = value
            .get("ts")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_unix)
        else {
            continue;
        };
        let fresher = match newest.get(arm) {
            Some(seen) => ts_unix >= seen.ts_unix,
            None => true,
        };
        if fresher {
            newest.insert(
                arm.to_string(),
                NewestTick {
                    ts_unix,
                    ts: value
                        .get("ts")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    data: Value::Object(data.clone()),
                },
            );
        }
    }
}

fn arm_status(
    arm: &str,
    default_interval_s: u64,
    newest: Option<&NewestTick>,
    now_unix: u64,
) -> ArmStatus {
    let Some(tick) = newest else {
        return ArmStatus {
            arm: arm.to_string(),
            scheduler: None,
            last_ts: None,
            age_s: None,
            acted: None,
            skip_reason: Some("never".to_string()),
            detail: None,
            interval_s: default_interval_s,
            stale: default_interval_s > 0,
        };
    };
    let interval_s = tick
        .data
        .get("interval_s")
        .and_then(Value::as_u64)
        .unwrap_or(default_interval_s);
    let age_s = now_unix.saturating_sub(tick.ts_unix);
    ArmStatus {
        arm: arm.to_string(),
        scheduler: str_field(&tick.data, "scheduler"),
        last_ts: Some(tick.ts.clone()),
        age_s: Some(age_s),
        acted: tick.data.get("acted").and_then(Value::as_u64),
        skip_reason: str_field(&tick.data, "skip_reason"),
        detail: str_field(&tick.data, "detail"),
        interval_s,
        stale: interval_s > 0 && age_s > interval_s * 2,
    }
}

fn str_field(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// Parse the two `ts` shapes the journals carry: second precision
/// (`2026-09-04T12:34:56Z`) and millisecond precision
/// (`2026-09-04T12:34:56.789Z`). Returns unix seconds.
pub(crate) fn parse_rfc3339_unix(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    let secs = days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64;
    Some(secs.max(0) as u64)
}

fn digits(bytes: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    for b in bytes {
        let digit = (*b as char).to_digit(10)? as u64;
        value = value * 10 + digit;
    }
    Some(value)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm), matching
/// `events::civil_from_unix` in reverse.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-tick-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn write_rows(path: &Path, rows: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::new();
        for row in rows {
            text.push_str(&serde_json::to_string(row).unwrap());
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }

    fn tick_envelope(ts: &str, arm: &str, acted: u64, skip: Value, interval_s: u64) -> Value {
        json!({
            "ts": ts,
            "type": EVENT_TYPE,
            "source": "loop",
            "data": {
                "arm": arm,
                "scheduler": "daemon",
                "acted": acted,
                "skip_reason": skip,
                "detail": null,
                "interval_s": interval_s,
            }
        })
    }

    #[test]
    fn emit_lands_one_row_in_the_journal() {
        let dir = temp_dir();
        let project = dir.join("events.jsonl");
        let journal = Journal::new_raw(project.clone(), dir.join("global.jsonl"));
        emit_tick(
            &journal,
            "active_backlog",
            "daemon",
            2,
            None,
            Some("mission=x"),
            300,
        );

        let text = std::fs::read_to_string(&project).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], EVENT_TYPE);
        assert_eq!(lines[0]["data"]["arm"], "active_backlog");
        assert_eq!(lines[0]["data"]["scheduler"], "daemon");
        assert_eq!(lines[0]["data"]["acted"], 2);
        assert_eq!(lines[0]["data"]["skip_reason"], Value::Null);
        assert_eq!(lines[0]["data"]["interval_s"], 300);
        assert!(lines[0]["ts"].as_str().unwrap().ends_with('Z'));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_row_per_arm_wins_across_journals_and_rotation() {
        let dir = temp_dir();
        let a = dir.join("global.jsonl");
        let a_rotated = dir.join("global.jsonl.1");
        let b = dir.join("agents.jsonl");
        write_rows(
            &a_rotated,
            &[tick_envelope(
                "2026-09-04T10:00:00Z",
                "king_wake",
                0,
                json!("no_crowned_target"),
                900,
            )],
        );
        write_rows(
            &a,
            &[tick_envelope(
                "2026-09-04T11:00:00Z",
                "king_wake",
                1,
                json!(null),
                900,
            )],
        );
        write_rows(
            &b,
            &[tick_envelope(
                "2026-09-04T11:00:00.500Z",
                "active_backlog",
                3,
                json!(null),
                300,
            )],
        );

        let now = parse_rfc3339_unix("2026-09-04T11:00:10Z").unwrap();
        let rows = read_arms(&[a, b], now);
        let king = rows.iter().find(|r| r.arm == "king_wake").unwrap();
        assert_eq!(king.acted, Some(1));
        assert_eq!(king.skip_reason, None);
        assert!(!king.stale);
        let ab = rows.iter().find(|r| r.arm == "active_backlog").unwrap();
        assert_eq!(ab.acted, Some(3));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_at_twice_the_interval_and_on_row_interval_override() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        // watchdog's table default is 600s, so 700s alone would read fresh.
        // The row claims interval 300, and 700 > 2x300 flips it stale: the
        // row's own interval, not the table default, drives the verdict.
        write_rows(
            &journal,
            &[tick_envelope(
                "2026-09-04T10:00:00Z",
                "watchdog",
                0,
                json!("watchdog_off"),
                300,
            )],
        );
        let now = parse_rfc3339_unix("2026-09-04T10:11:40Z").unwrap(); // 700s later
        let rows = read_arms(&[journal.clone()], now);
        let wd = rows.iter().find(|r| r.arm == "watchdog").unwrap();
        assert_eq!(wd.skip_reason.as_deref(), Some("watchdog_off"));
        assert!(
            wd.stale,
            "700s against the row's own 300s interval is stale"
        );

        // 599s against the same row: under 2x300, fresh.
        let now_earlier = parse_rfc3339_unix("2026-09-04T10:09:59Z").unwrap();
        let rows = read_arms(&[journal], now_earlier);
        let wd = rows.iter().find(|r| r.arm == "watchdog").unwrap();
        assert!(!wd.stale);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_ticked_arm_is_stale_and_every_arm_appears() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&journal, "").unwrap();
        let rows = read_arms(&[journal.clone()], 1_800_000_000);
        assert!(rows.len() >= KNOWN_ARMS.len());
        for spec in KNOWN_ARMS {
            let row = rows.iter().find(|r| r.arm == spec.arm).unwrap();
            if spec.default_interval_s == 0 {
                assert!(
                    !row.stale,
                    "event-driven arm {} never reads stale",
                    spec.arm
                );
            } else {
                assert!(row.stale, "never-ticked arm {} reads stale", spec.arm);
                assert_eq!(row.skip_reason.as_deref(), Some("never"));
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ts_parser_covers_both_shapes_and_known_points() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix("2026-09-04T12:00:00Z"),
            parse_rfc3339_unix("2026-09-04T12:00:00.123Z")
        );
        assert_eq!(
            parse_rfc3339_unix("2026-09-04T12:00:00Z"),
            Some(1_788_523_200)
        );
        assert_eq!(parse_rfc3339_unix("not-a-ts"), None);
    }
}

//! `fno doctor event signals`: read the store for patterns that point at a
//! fault. One row of telemetry is noise; the shape of many rows is the
//! signal. The verb flags four shapes over a window (24h by default):
//!
//! - `burst`: one event type wrote at least `burst_rows` rows in one minute.
//! - `spike`: a type wrote at least `spike_rows` rows in the window and at
//!   least `spike_factor` times its average per window over the prior week.
//! - `arm_errors`: a control-plane arm reported a failure in its tick
//!   detail at least `arm_error_ticks` times.
//! - `daemon_restarts`: the daemon started (`keeper_sweep_done` or
//!   `keeper_sweep_failed`) more than `restarts` times.
//!
//! Read only: it opens the store read-only and never creates or prunes it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Serialize;

const HOUR_MS: i64 = 3_600_000;

/// How many prior windows the spike baseline averages over.
const BASELINE_WINDOWS: i64 = 6;

/// The thresholds a finding must reach. The defaults are set so a healthy
/// fleet reads clean; a test passes small ones.
pub(crate) struct Limits {
    pub window_ms: i64,
    pub burst_rows: i64,
    pub spike_rows: i64,
    pub spike_factor: i64,
    pub arm_error_ticks: i64,
    pub restarts: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            window_ms: 24 * HOUR_MS,
            burst_rows: 1_000,
            spike_rows: 1_000,
            spike_factor: 5,
            arm_error_ticks: 10,
            restarts: 12,
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct Signal {
    pub signal: &'static str,
    pub subject: String,
    pub count: i64,
    pub detail: String,
}

/// Every signal in the window ending at `now_ms`, worst count first within
/// each shape.
pub(crate) fn read_signals(
    store: &Path,
    now_ms: i64,
    limits: &Limits,
) -> Result<Vec<Signal>, String> {
    let conn = crate::event_store::open_read(store)?;
    let named = |e: rusqlite::Error| format!("{}: {e}", store.display());
    let since = now_ms - limits.window_ms;
    let mut out = Vec::new();

    let mut stmt = conn
        .prepare(
            "SELECT type, max(c) FROM (SELECT type, ts_ms / 60000 AS m, count(*) AS c \
             FROM events WHERE ts_ms >= ?1 AND reject_reason IS NULL GROUP BY type, m) \
             GROUP BY type HAVING max(c) >= ?2 ORDER BY 2 DESC",
        )
        .map_err(named)?;
    let rows = stmt
        .query_map(params![since, limits.burst_rows], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(named)?;
    for row in rows {
        let (ty, c) = row.map_err(named)?;
        out.push(Signal {
            signal: "burst",
            subject: ty,
            count: c,
            detail: "rows in one minute".to_string(),
        });
    }

    // The baseline only counts windows the store actually covers, so a young
    // store never reads its whole history as a spike.
    let oldest: Option<i64> = conn
        .query_row("SELECT min(ts_ms) FROM events", [], |r| r.get(0))
        .map_err(named)?;
    let base_start = (since - BASELINE_WINDOWS * limits.window_ms).max(oldest.unwrap_or(since));
    let windows = (since - base_start) / limits.window_ms;
    if windows > 0 {
        let mut stmt = conn
            .prepare(
                "SELECT type, sum(ts_ms >= ?1), sum(ts_ms < ?1) FROM events \
                 WHERE ts_ms >= ?2 AND reject_reason IS NULL GROUP BY type \
                 HAVING sum(ts_ms >= ?1) >= ?3 ORDER BY 2 DESC",
            )
            .map_err(named)?;
        let rows = stmt
            .query_map(params![since, base_start, limits.spike_rows], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(named)?;
        for row in rows {
            let (ty, current, base) = row.map_err(named)?;
            let average = base / windows;
            if current >= limits.spike_factor * average.max(1) {
                out.push(Signal {
                    signal: "spike",
                    subject: ty,
                    count: current,
                    detail: format!("prior average {average} per window"),
                });
            }
        }
    }

    let mut stmt = conn
        .prepare(
            "SELECT json_extract(line, '$.data.arm') AS arm, count(*), \
             max(substr(json_extract(line, '$.data.detail'), 1, 200)) FROM events \
             WHERE type = 'control_plane_tick' AND ts_ms >= ?1 AND reject_reason IS NULL \
             AND (lower(coalesce(json_extract(line, '$.data.detail'), '')) LIKE '%failed%' \
               OR lower(coalesce(json_extract(line, '$.data.detail'), '')) LIKE '%error%') \
             GROUP BY arm HAVING count(*) >= ?2 ORDER BY 2 DESC",
        )
        .map_err(named)?;
    let rows = stmt
        .query_map(params![since, limits.arm_error_ticks], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(named)?;
    for row in rows {
        let (arm, c, sample) = row.map_err(named)?;
        out.push(Signal {
            signal: "arm_errors",
            subject: arm.unwrap_or_else(|| "unknown".to_string()),
            count: c,
            detail: sample.unwrap_or_default(),
        });
    }

    let restarts: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE type IN ('keeper_sweep_done', \
             'keeper_sweep_failed') AND ts_ms >= ?1 AND reject_reason IS NULL",
            params![since],
            |r| r.get(0),
        )
        .map_err(named)?;
    if restarts > limits.restarts {
        out.push(Signal {
            signal: "daemon_restarts",
            subject: "daemon".to_string(),
            count: restarts,
            detail: "daemon starts in the window".to_string(),
        });
    }
    Ok(out)
}

/// `signals [--events <events.jsonl>] [--window-hours N] [--check]`. Prints
/// one JSON receipt. Exit 0 on a read, 3 with `--check` when any signal
/// fired, 1 when the store cannot be read.
pub(crate) fn run(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut limits = Limits::default();
    let mut check = false;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        match tok.to_str() {
            Some("--events") => journal = it.next().map(PathBuf::from),
            Some("--window-hours") => {
                match it.next().and_then(|v| v.to_str()?.parse::<i64>().ok()) {
                    Some(h) if h > 0 => limits.window_ms = h * HOUR_MS,
                    _ => {
                        eprintln!("error: --window-hours takes a positive integer");
                        return 2;
                    }
                }
            }
            Some("--check") => check = true,
            _ => {}
        }
    }
    let journal = journal.unwrap_or_else(crate::pane_send_audit::pane_send_audit_events_path);
    let store = crate::event_store::store_path(&journal);
    if !store.is_file() {
        eprintln!("error: store {} does not exist", store.display());
        return 1;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    match read_signals(&store, now_ms, &limits) {
        Ok(signals) => {
            let receipt = serde_json::json!({
                "store": store.display().to_string(),
                "window_hours": limits.window_ms / HOUR_MS,
                "clean": signals.is_empty(),
                "signals": signals,
            });
            println!("{receipt}");
            if check && !signals.is_empty() {
                3
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_shape_fires_on_its_pattern_and_a_quiet_store_reads_clean() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("events.db");
        let mut conn = rusqlite::Connection::open(&store).unwrap();
        crate::event_store::ensure_schema(&mut conn, &store).unwrap();
        let now = 10 * 24 * HOUR_MS;
        let mut n = 0;
        let mut insert = |ty: &str, ts: i64, data: serde_json::Value| {
            n += 1;
            let line = serde_json::json!({"type": ty, "data": data}).to_string();
            conn.execute(
                "INSERT INTO events (event_id, row_hash, ts_ms, type, source, line) \
                 VALUES (?1, ?2, ?3, ?4, 'test', ?5)",
                params![format!("e{n}"), format!("h{n}").into_bytes(), ts, ty, line],
            )
            .unwrap();
        };
        // A week of one quiet row a day, so the baseline covers six windows.
        for day in 1..=8 {
            insert(
                "lead_checkin",
                now - day * 24 * HOUR_MS,
                serde_json::json!({}),
            );
        }
        let limits = Limits {
            burst_rows: 5,
            spike_rows: 5,
            arm_error_ticks: 3,
            restarts: 2,
            ..Limits::default()
        };
        assert_eq!(read_signals(&store, now, &limits).unwrap(), vec![]);

        for _ in 0..6 {
            insert(
                "store_seat_lock_unlinked",
                now - 30_000,
                serde_json::json!({}),
            );
        }
        for _ in 0..3 {
            insert(
                "control_plane_tick",
                now - HOUR_MS,
                serde_json::json!({"arm": "merge_close", "detail": "sync_catchup=failed: exit 1"}),
            );
            insert("keeper_sweep_done", now - HOUR_MS, serde_json::json!({}));
        }
        let shapes: Vec<(&str, String)> = read_signals(&store, now, &limits)
            .unwrap()
            .into_iter()
            .map(|s| (s.signal, s.subject))
            .collect();
        assert!(
            shapes.contains(&("burst", "store_seat_lock_unlinked".into())),
            "{shapes:?}"
        );
        assert!(
            shapes.contains(&("spike", "store_seat_lock_unlinked".into())),
            "{shapes:?}"
        );
        assert!(
            shapes.contains(&("arm_errors", "merge_close".into())),
            "{shapes:?}"
        );
        assert!(
            shapes.contains(&("daemon_restarts", "daemon".into())),
            "{shapes:?}"
        );
        assert!(
            !shapes.iter().any(|(_, s)| s == "lead_checkin"),
            "{shapes:?}"
        );
    }
}

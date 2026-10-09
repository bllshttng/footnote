//! `fno doctor event signals`: read the store for patterns that point at a
//! fault. One row of telemetry is noise; the shape of many rows is the
//! signal. The verb flags four shapes over a window (24h by default):
//!
//! - `burst`: one event type wrote at least `burst_rows` rows in one minute.
//! - `spike`: a type wrote at least `spike_rows` rows in the window and at
//!   least `spike_factor` times its average per window over the six prior
//!   windows it has rows for (a type with no prior rows reads as a spike).
//! - `arm_errors`: a control-plane arm reported a failure at least
//!   `arm_error_ticks` times: a failing `skip_reason` token, or a tick detail
//!   carrying `failed:`, `=failed`, `error:` or `os error`.
//! - `daemon_restarts`: the daemon started (`keeper_sweep_done` or
//!   `keeper_sweep_failed`) more than `restarts` times.
//!
//! Read only: it opens the store read-only and never creates or prunes it.
//! Every query seeks the `(type, ts_ms)` index, one type at a time, so the
//! cost tracks the window, not the whole history.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
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

/// Every distinct type, by skip-scan over the `(type, ts_ms)` index: one
/// seek per type instead of one step per row.
fn distinct_types(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE t(ty) AS (SELECT min(type) FROM events \
         UNION ALL SELECT (SELECT min(type) FROM events WHERE type > t.ty) \
         FROM t WHERE t.ty IS NOT NULL) SELECT ty FROM t WHERE ty IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Every signal in the window ending at `now_ms`, grouped by shape.
pub(crate) fn read_signals(
    store: &Path,
    now_ms: i64,
    limits: &Limits,
) -> Result<Vec<Signal>, String> {
    let conn = crate::event_store::open_read(store)?;
    let named = |e: rusqlite::Error| format!("{}: {e}", store.display());
    let since = now_ms - limits.window_ms;
    let base_floor = since - BASELINE_WINDOWS * limits.window_ms;
    let mut bursts = Vec::new();
    let mut spikes = Vec::new();
    for ty in distinct_types(&conn).map_err(named)? {
        let peak: i64 = conn
            .query_row(
                "SELECT coalesce(max(c), 0) FROM (SELECT count(*) AS c FROM events \
                 WHERE type = ?1 AND ts_ms >= ?2 AND reject_reason IS NULL \
                 GROUP BY ts_ms / 60000)",
                params![ty, since],
                |r| r.get(0),
            )
            .map_err(named)?;
        if peak >= limits.burst_rows {
            bursts.push(Signal {
                signal: "burst",
                subject: ty.clone(),
                count: peak,
                detail: "rows in one minute".to_string(),
            });
        }
        let (current, base, oldest): (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT coalesce(sum(ts_ms >= ?2), 0), coalesce(sum(ts_ms < ?2), 0), \
                 min(ts_ms) FROM events WHERE type = ?1 AND ts_ms >= ?3 \
                 AND reject_reason IS NULL",
                params![ty, since, base_floor],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(named)?;
        if current < limits.spike_rows {
            continue;
        }
        // The baseline spans only the windows this type has rows in, so a
        // telemetry kind kept for a week never averages over empty days.
        let windows = (since - oldest.unwrap_or(since).max(base_floor)) / limits.window_ms;
        let average = if windows > 0 { base / windows } else { 0 };
        if current >= limits.spike_factor * average.max(1) {
            spikes.push(Signal {
                signal: "spike",
                subject: ty,
                count: current,
                detail: if windows > 0 {
                    format!("prior average {average} per window")
                } else {
                    "no prior rows".to_string()
                },
            });
        }
    }
    let mut out = bursts;
    out.append(&mut spikes);

    // A failure is a failing skip_reason token or an error marker in the
    // detail. Counters such as `failed=0` or `errors=0` match neither.
    let mut stmt = conn
        .prepare(
            "SELECT arm, count(*), max(detail) FROM (SELECT \
             json_extract(line, '$.data.arm') AS arm, \
             lower(coalesce(json_extract(line, '$.data.skip_reason'), '')) AS skip, \
             substr(coalesce(json_extract(line, '$.data.detail'), ''), 1, 200) AS detail \
             FROM events WHERE type = 'control_plane_tick' AND ts_ms >= ?1 \
             AND reject_reason IS NULL) \
             WHERE skip LIKE '%fail%' OR skip LIKE '%error%' OR skip LIKE '%timeout%' \
             OR skip LIKE '%broken%' OR skip LIKE '%unreadable%' \
             OR lower(detail) LIKE '%failed:%' OR lower(detail) LIKE '%=failed%' \
             OR lower(detail) LIKE '%error:%' OR lower(detail) LIKE '%os error%' \
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

const USAGE: &str = "usage: fno doctor event signals [--events <events.jsonl>] \
[--window-hours N] [--pruned-days N] [--check]";

/// `signals [--events <events.jsonl>] [--window-hours N] [--pruned-days N]
/// [--check]`. Prints one JSON receipt. Its `pruned` list totals what the
/// telemetry prune deleted over the last `--pruned-days` (30 by default), per
/// kind and subject: the rows are gone, the counts stay. Exit 0 on a read, 3
/// with `--check` when any signal fired, 2 on a bad argument, 1 when the
/// store cannot be read.
pub(crate) fn run(args: &[OsString]) -> i32 {
    let mut journal: Option<PathBuf> = None;
    let mut limits = Limits::default();
    let mut pruned_days: i64 = 30;
    let mut check = false;
    let mut it = args.iter();
    while let Some(tok) = it.next() {
        match tok.to_str() {
            Some("--events") => match it.next() {
                Some(v) => journal = Some(PathBuf::from(v)),
                None => {
                    eprintln!("error: --events takes a journal path\n{USAGE}");
                    return 2;
                }
            },
            Some("--window-hours") => {
                match it.next().and_then(|v| v.to_str()?.parse::<i64>().ok()) {
                    Some(h) if h > 0 => limits.window_ms = h * HOUR_MS,
                    _ => {
                        eprintln!("error: --window-hours takes a positive integer\n{USAGE}");
                        return 2;
                    }
                }
            }
            Some("--pruned-days") => {
                match it.next().and_then(|v| v.to_str()?.parse::<i64>().ok()) {
                    Some(d) if d > 0 => pruned_days = d,
                    _ => {
                        eprintln!("error: --pruned-days takes a positive integer\n{USAGE}");
                        return 2;
                    }
                }
            }
            Some("--check") => check = true,
            _ => {
                eprintln!("error: unknown argument {tok:?}\n{USAGE}");
                return 2;
            }
        }
    }
    let journal = journal.unwrap_or_else(crate::pane_send_audit::pane_send_audit_events_path);
    let store = crate::event_store::store_path(&journal);
    if !store.is_file() {
        eprintln!("error: store {} does not exist", store.display());
        return 1;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let since_day = (chrono::Utc::now() - chrono::Duration::days(pruned_days))
        .format("%Y-%m-%d")
        .to_string();
    let pruned = crate::event_store::open_read(&store)
        .and_then(|conn| crate::event_store::read_rollup(&conn, &since_day));
    match read_signals(&store, now_ms, &limits).and_then(|s| Ok((s, pruned?))) {
        Ok((signals, pruned)) => {
            let receipt = serde_json::json!({
                "store": store.display().to_string(),
                "window_hours": limits.window_ms / HOUR_MS,
                "clean": signals.is_empty(),
                "signals": signals,
                "pruned_days": pruned_days,
                "pruned": pruned,
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
        // A healthy counter in a tick detail is not a failure.
        for _ in 0..3 {
            insert(
                "control_plane_tick",
                now - HOUR_MS,
                serde_json::json!({"arm": "reap", "detail": "failed=0 errors=0"}),
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
        for want in [
            ("burst", "store_seat_lock_unlinked"),
            ("spike", "store_seat_lock_unlinked"),
            ("arm_errors", "merge_close"),
            ("daemon_restarts", "daemon"),
        ] {
            assert!(
                shapes.contains(&(want.0, want.1.to_string())),
                "{want:?} missing: {shapes:?}"
            );
        }
        assert!(
            !shapes
                .iter()
                .any(|(_, s)| s == "lead_checkin" || s == "reap"),
            "{shapes:?}"
        );
    }
}

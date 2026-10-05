//! The two renders of one status computation: the one-line JSON object the
//! SessionStart hook, the doctor and the scripts read (Python `json.dumps`
//! byte shape, key order included), and the human block the Python `status`
//! printed. Both take the same verdict + marks; neither re-reads the world.

use super::status::{
    as_bool, cfg_lookup, state_root, tick_end_bits, Inputs, Marks, MergeScan, Verdict,
    PLIST_FILENAME,
};
use serde_json::Value;

/// One JSON scalar, kept in insertion order the way the Python dict was.
enum Jv {
    S(String),
    B(bool),
    N,
    Raw(String),
    O(Vec<(&'static str, Jv)>),
}

impl Jv {
    fn s(v: Option<&str>) -> Jv {
        match v {
            Some(s) => Jv::S(s.to_string()),
            None => Jv::N,
        }
    }
}

/// Python `json.dumps` byte shape: `", "` separators, `": "` after keys,
/// `ensure_ascii` escapes, lowercase hex.
fn python_json(v: &Jv) -> String {
    match v {
        Jv::S(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for unit in s.encode_utf16() {
                match unit {
                    0x22 => out.push_str("\\\""),
                    0x5c => out.push_str("\\\\"),
                    0x0a => out.push_str("\\n"),
                    0x0d => out.push_str("\\r"),
                    0x09 => out.push_str("\\t"),
                    0x20..=0x7e => out.push(unit as u8 as char),
                    0x00..=0x1f => out.push_str(&format!("\\u{unit:04x}")),
                    _ => out.push_str(&format!("\\u{unit:04x}")),
                }
            }
            out.push('"');
            out
        }
        Jv::B(b) => b.to_string(),
        Jv::N => "null".to_string(),
        Jv::Raw(raw) => raw.clone(),
        Jv::O(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(k, v)| format!("\"{k}\": {}", python_json(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn value_jv(value: &Value) -> Jv {
    match value {
        Value::String(s) => Jv::S(s.clone()),
        Value::Bool(b) => Jv::B(*b),
        Value::Null => Jv::N,
        Value::Number(n) => Jv::Raw(n.to_string()),
        other => Jv::S(other.to_string()),
    }
}

fn merge_scan_jv(scan: &MergeScan) -> Jv {
    Jv::O(vec![
        ("completed", Jv::B(scan.completed)),
        ("completed_at", Jv::S(scan.completed_at.clone())),
        ("scanned", value_jv(&scan.scanned)),
    ])
}

/// The watchdog freshness read: `lane_armed` + `sweep_staleness` ported from
/// `fno.agents.watchdog`. `None` arm means the lane is off on purpose or its
/// keys are unreadable, and the block stays silent - a freshness reader that
/// does not share the producer's own gate once printed STALE for a cadence
/// that was off on purpose.
struct SweepStaleness {
    age_s: Option<i64>,
    stale: bool,
    source: Option<String>,
    at: Option<String>,
}

fn watchdog_armed(cwd: &std::path::Path) -> bool {
    watchdog_gate(
        cfg_lookup(cwd, &["recovery", "watchdog", "enabled"]).map(|v| as_bool(Some(v), false)),
        cfg_lookup(cwd, &["recovery", "enabled"]).map(|v| as_bool(Some(v), true)),
        cfg_lookup(cwd, &["autonomy", "enabled"]).map(|v| as_bool(Some(v), true)),
    )
}

/// The lane's arming question, pure over the three keys so the contract stays
/// unit-testable without a config: the watchdog switch must read ON, and
/// neither the recovery sweep nor the autonomy master switch may read OFF.
/// A missing key takes the same default the Python loader gave it.
fn watchdog_gate(watchdog: Option<bool>, recovery: Option<bool>, autonomy: Option<bool>) -> bool {
    watchdog.unwrap_or(false) && recovery.unwrap_or(true) && autonomy.unwrap_or(true)
}

fn sweep_staleness(cwd: &std::path::Path, now: f64, stale_after_s: f64) -> SweepStaleness {
    sweep_staleness_at(
        &state_root(cwd).join("watchdog-sweep.json"),
        now,
        stale_after_s,
    )
}

fn sweep_staleness_at(path: &std::path::Path, now: f64, stale_after_s: f64) -> SweepStaleness {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SweepStaleness {
            age_s: None,
            stale: true,
            source: None,
            at: None,
        };
    };
    let mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64());
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return SweepStaleness {
            age_s: None,
            stale: true,
            source: None,
            at: None,
        };
    };
    let source = data
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let at = data
        .get("at")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let tick_epoch = data.get("last_tick_epoch").and_then(Value::as_f64);
    let age = match tick_epoch {
        Some(epoch) => (now - epoch).max(0.0),
        None => {
            if source == "tick" {
                match mtime {
                    Some(mtime) => (now - mtime).max(0.0),
                    None => {
                        return SweepStaleness {
                            age_s: None,
                            stale: true,
                            source: Some(source),
                            at: Some(at),
                        }
                    }
                }
            } else {
                // No cadence evidence at all: a hand-run is the only sweep on
                // record, and it proves nothing about the launchd cadence.
                return SweepStaleness {
                    age_s: mtime.map(|mtime| (now - mtime) as i64),
                    stale: true,
                    source: Some(source),
                    at: Some(at),
                };
            }
        }
    };
    SweepStaleness {
        age_s: Some(age as i64),
        stale: age > stale_after_s,
        source: Some(source),
        at: Some(at),
    }
}

/// The full JSON object: `enabled, verdict, detail, fix, loaded, last_tick,
/// interval_seconds, bounce_pending`, then `merge_scan` when the newest tick
/// carried a scan, then `watchdog` when the lane is armed.
pub(crate) fn build_json(verdict: &Verdict, marks: &Marks, inp: &Inputs) -> String {
    let mut fields: Vec<(&'static str, Jv)> = vec![
        ("enabled", Jv::B(verdict.enabled)),
        ("verdict", Jv::S(verdict.word.clone())),
        ("detail", Jv::S(verdict.detail.clone())),
        (
            "fix",
            match &verdict.fix {
                Some(fix) => Jv::S(fix.clone()),
                None => Jv::N,
            },
        ),
        ("loaded", Jv::B(inp.loaded)),
        ("last_tick", Jv::s(marks.last_tick.as_deref())),
        (
            "interval_seconds",
            Jv::Raw(inp.cfg.interval_seconds.to_string()),
        ),
        ("bounce_pending", Jv::B(verdict.bounce_pending)),
    ];
    if let Some(scan) = &marks.merge_scan {
        fields.push(("merge_scan", merge_scan_jv(scan)));
    }
    if watchdog_armed(&inp.cwd) {
        let sweep = sweep_staleness(
            &inp.cwd,
            inp.now,
            (2 * inp.cfg.interval_seconds.max(1)) as f64,
        );
        fields.push((
            "watchdog",
            Jv::O(vec![
                (
                    "age_s",
                    match sweep.age_s {
                        Some(age) => Jv::Raw(age.to_string()),
                        None => Jv::N,
                    },
                ),
                ("stale", Jv::B(sweep.stale)),
                ("source", Jv::s(sweep.source.as_deref())),
                ("at", Jv::s(sweep.at.as_deref())),
            ]),
        ));
    }
    python_json(&Jv::O(fields))
}

/// The `Heal:` line comes from the healer's own renderer: one readout, two
/// callers, no second spelling of it.
fn heal_line(inp: &Inputs) -> String {
    let armed = as_bool(cfg_lookup(&inp.cwd, &["auto_heal", "enabled"]), false);
    crate::heal::status_readout(armed, &inp.events)
}

/// The open-PR count from the cache the tick last measured.
fn observed_open_pr_count(path: &std::path::Path) -> i64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };
    data.values()
        .filter(|entry| entry.get("last_seen_state").and_then(Value::as_str) == Some("OPEN"))
        .count() as i64
}

/// The parked block, read by the one owner: the in-process `pr-park` list.
fn parked_block(inp: &Inputs) -> String {
    let mut out = String::new();
    let paths = crate::pr_park::Paths::resolve(&inp.cwd);
    let ctx = crate::pr_park::Ctx::live(&inp.cwd, paths);
    let rows = crate::pr_park::list_rows(&ctx);
    if rows.is_empty() {
        out.push_str("Parked PRs:   none\n");
        return out;
    }
    let open_rows: Vec<_> = rows.iter().filter(|r| r.bucket == "open").collect();
    let finished = rows.iter().filter(|r| r.bucket == "finished").count();
    let foreign = rows.iter().filter(|r| r.bucket == "foreign").count();
    out.push_str(&format!(
        "Parked PRs ({} open, {finished} finished, {foreign} foreign):\n",
        open_rows.len()
    ));
    for row in open_rows {
        let age = if row.age_hours < 0 {
            "?".to_string()
        } else {
            format!("{}h", row.age_hours)
        };
        let detail = if !row.reason_detail.is_empty() {
            row.reason_detail.as_str()
        } else if !row.reason.is_empty() {
            row.reason.as_str()
        } else {
            ""
        };
        let node = if row.node.is_empty() {
            "-"
        } else {
            row.node.as_str()
        };
        let node_status = if row.node_status.is_empty() {
            "no node status"
        } else {
            row.node_status.as_str()
        };
        out.push_str(&format!(
            "  {}  {detail} ({age}, node {node}, {node_status})\n",
            row.key
        ));
    }
    if finished > 0 {
        out.push_str(&format!(
            "  finished: {finished} (the sweep marks these handled)\n"
        ));
    }
    if foreign > 0 {
        out.push_str(&format!("  foreign: {foreign} (other repos, left alone)\n"));
    }
    out
}

/// The human readout: stdout text plus the watchdog STALE line, which goes to
/// stderr the way the Python `typer.echo(..., err=True)` sent it.
pub(crate) fn render_text(inp: &Inputs, marks: &Marks, verdict: &Verdict) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    let plist_path = inp.launch_agents_dir.join(PLIST_FILENAME);
    out.push_str(&format!(
        "Agent loaded: {}\n",
        if inp.loaded { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "Plist path:   {} ({})\n",
        plist_path.display(),
        if plist_path.exists() {
            "exists"
        } else {
            "missing"
        }
    ));
    out.push_str(&format!(
        "Verdict:      {} ({})\n",
        verdict.word, verdict.detail
    ));
    if let Some(fix) = &verdict.fix {
        out.push_str(&format!("Fix:          {fix}\n"));
    }
    out.push_str(&heal_line(inp));
    out.push('\n');
    out.push_str(&format!(
        "Last tick:    {}\n",
        marks.last_tick.as_deref().unwrap_or("(no tick recorded)")
    ));
    out.push_str(&format!(
        "Last attempt: {}\n",
        marks
            .last_attempt
            .as_deref()
            .unwrap_or("(no attempt recorded)")
    ));
    match &marks.last_end {
        None => out.push_str("Last tick outcome: (no end record)\n"),
        Some(end) => {
            let bits = tick_end_bits(end);
            let detail = if bits.is_empty() {
                String::new()
            } else {
                format!(" ({})", bits.join(", "))
            };
            out.push_str(&format!(
                "Last tick outcome: {}{}\n",
                end.outcome.clone().unwrap_or_default(),
                detail
            ));
        }
    }
    match &marks.completed_tick {
        None => out.push_str("Completed tick: none\n"),
        Some((ts, swept)) => out.push_str(&format!("Completed tick: {ts} swept={swept}\n")),
    }
    match &marks.merge_scan {
        Some(scan) => out.push_str(&format!(
            "Merge scan:   completed_at={} scanned={}\n",
            scan.completed_at, scan.scanned
        )),
        None => out.push_str("Merge scan:   (no scan receipt from a merge_scan-capable tick)\n"),
    }
    if watchdog_armed(&inp.cwd) {
        let sweep = sweep_staleness(
            &inp.cwd,
            inp.now,
            (2 * inp.cfg.interval_seconds.max(1)) as f64,
        );
        let age = match sweep.age_s {
            Some(age) => format!("{}m", age / 60),
            None => "?".to_string(),
        };
        if sweep.stale {
            err.push_str(&format!(
                "FLEET WATCHDOG STALE: last sweep {age} old (interval {}s, source {}). \
                 A dead cadence reads as a healthy fleet. \
                 Sweep manually: fno agents watchdog\n",
                inp.cfg.interval_seconds,
                sweep.source.as_deref().unwrap_or("none")
            ));
        } else {
            out.push_str(&format!(
                "Watchdog:     fresh ({age} old, source {})\n",
                sweep.source.as_deref().unwrap_or_default()
            ));
        }
    }
    out.push_str(&format!(
        "Open PRs:     {}\n",
        observed_open_pr_count(&inp.watcher_state)
    ));
    out.push_str(&parked_block(inp));
    (out, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_gate_needs_the_switch_on_and_no_master_off() {
        // The lane reads armed only when the watchdog switch is explicitly
        // on and neither the recovery sweep nor the autonomy master is off.
        assert!(watchdog_gate(Some(true), Some(true), Some(true)));
        assert!(!watchdog_gate(Some(false), Some(true), Some(true)));
        assert!(!watchdog_gate(None, Some(true), Some(true)));
        assert!(!watchdog_gate(Some(true), Some(false), Some(true)));
        assert!(!watchdog_gate(Some(true), Some(true), Some(false)));
        // Missing recovery/autonomy keys default to their loader defaults
        // (both on); a missing watchdog key defaults off.
        assert!(watchdog_gate(Some(true), None, None));
        assert!(!watchdog_gate(None, None, None));
    }

    #[test]
    fn sweep_staleness_reads_the_tick_epoch_not_the_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("watchdog-sweep.json");
        // No sweep ever ran: the loudest case, not a clean one.
        let missing = sweep_staleness_at(&path, 10_000.0, 1_200.0);
        assert!(missing.stale);
        assert_eq!(missing.age_s, None);
        assert_eq!(missing.source, None);

        // A tick-sourced sweep: the age measures the last TICK, so a
        // hand-touched file cannot hide a dead cadence.
        std::fs::write(
            &path,
            r#"{"source": "tick", "at": "x", "last_tick_epoch": 9500}"#,
        )
        .unwrap();
        let fresh = sweep_staleness_at(&path, 10_000.0, 1_200.0);
        assert!(!fresh.stale);
        assert_eq!(fresh.age_s, Some(500));

        let stale = sweep_staleness_at(&path, 12_000.0, 1_200.0);
        assert!(stale.stale);
        assert_eq!(stale.age_s, Some(2500));
    }
}

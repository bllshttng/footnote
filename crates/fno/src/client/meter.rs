//! The whole-machine resource meter: one `macmon` sample per refresh for CPU
//! and watts; the memory figure and the capacity line come from the learned
//! capacity state the machine_watch tick writes (one reader, one number).
//! `config.mux.load_readout` picks plain words (the default) or the raw
//! numbers; the sampler re-reads it each time the meter is switched on.

use std::path::PathBuf;

/// Spawn the meter sampler: one bounded `macmon pipe -s 1` sample per refresh
/// interval, the one-line reading sent to the UI loop. Exits when the view's
/// gate flips off, so a toggle-off never leaves a sampler running. Two
/// overlapping tasks are harmless: the channel is last-send-wins.
pub(super) fn spawn_meter_sampler(
    gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    refresh: u64,
    meter_tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let detailed = crate::digest_overlay::load_readout_detailed(&cwd);
    tokio::spawn(async move {
        while gate.load(std::sync::atomic::Ordering::Relaxed) {
            let text = sample_macmon_line(detailed).await;
            if meter_tx.send(text).is_err() {
                // The UI loop is gone; nothing left to report to.
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(refresh)).await;
        }
    });
}

/// One bounded `macmon pipe -s 1` sample rendered as a status-row segment.
/// macmon streams forever, so the timeout is the normal exit; anything that
/// fails to arrive or parse renders as "sensor unavailable" - a dark sensor
/// is named, never read as a zero.
async fn sample_macmon_line(detailed: bool) -> String {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        tokio::process::Command::new("macmon")
            .arg("pipe")
            .arg("-s")
            .arg("1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let parsed = match output {
        Ok(Ok(out)) => parse_macmon_sample(&out.stdout, detailed),
        _ => None,
    };
    match parsed {
        Some(cpu) => {
            let state = capacity_state();
            let mut line = cpu;
            line.push_str(&memory_segment(state.as_ref(), detailed));
            line.push_str(&capacity_segment(state.as_ref()));
            line
        }
        None => "meter: sensor unavailable".into(),
    }
}

/// CPU and watts only: the memory figure reads from the capacity state, so
/// the meter and the org overlay can never disagree about it.
fn parse_macmon_sample(raw: &[u8], detailed: bool) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let line = text.lines().find(|l| l.trim_start().starts_with('{'))?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let cpu = value.get("cpu_usage_pct")?.as_f64()?;
    // macmon's measured contract is a 0-1 fraction; no percent spelling to
    // rescue (the lanes arm pins the same contract).
    let cpu_pct = cpu * 100.0;
    let watts = value.get("sys_power").and_then(|p| p.as_f64());
    let mut line = if detailed {
        format!("cpu {cpu_pct:.0}%")
    } else {
        format!("CPU {cpu_pct:.0}% busy")
    };
    if let Some(w) = watts {
        if detailed {
            line.push_str(&format!(" {w:.0}W"));
        } else {
            line.push_str(&format!(" · {w:.0} W"));
        }
    }
    Some(line)
}

/// The capacity state file the tick writes, read through the same home chain
/// the registry resolves: `FNO_AGENTS_HOME` > `$HOME/.fno/agents`, the
/// machine-keyed file the fno-agents tick persists. `None` when missing or
/// unreadable: the gauge says "cap unknown", never a stale guess.
pub(crate) fn capacity_state() -> Option<serde_json::value::Value> {
    // Under test with no declared home there IS no state to read: the
    // ambient `$HOME/.fno/agents` is the real machine's, and a gauge test
    // must never depend on it.
    if cfg!(test) && std::env::var_os("FNO_AGENTS_HOME").is_none() {
        return None;
    }
    let root = match std::env::var_os("FNO_AGENTS_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno").join("agents"))?,
    };
    let mut key = crate::agents_view::machine_id();
    if key.is_empty() {
        key = crate::agents_view::hostname();
    }
    if key.is_empty() {
        key = "unknown-host".into();
    }
    let raw = std::fs::read_to_string(root.join("capacity").join(format!("{key}.json"))).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The memory segment: the state's own figure, or nothing - a missing
/// reader renders no number, never a zero.
fn memory_segment(state: Option<&serde_json::Value>, detailed: bool) -> String {
    let Some(state) = state else {
        return String::new();
    };
    let Some(mem) = state.get("memory") else {
        return String::new();
    };
    let (Some(total), Some(fraction)) = (
        mem.get("total_gb").and_then(|v| v.as_f64()),
        mem.get("used_fraction").and_then(|v| v.as_f64()),
    ) else {
        return String::new();
    };
    if detailed {
        let used = mem
            .get("compressor_occupied_gb")
            .and_then(|v| v.as_f64())
            .unwrap_or(fraction * total);
        format!(" · mem {used:.0}G/{total:.0}G")
    } else {
        format!(" · memory {:.0}% full", fraction * 100.0)
    }
}

/// The capacity segment: workers live, the learned cap under its ceiling,
/// then each visible account's windows. No state renders "cap unknown" and
/// no number (AC6).
fn capacity_segment(state: Option<&serde_json::Value>) -> String {
    let Some(state) = state else {
        return " · cap unknown".into();
    };
    let (Some(effective), Some(ceiling)) = (
        state.get("effective").and_then(|v| v.as_u64()),
        state.get("ceiling").and_then(|v| v.as_u64()),
    ) else {
        return " · cap unknown".into();
    };
    let mut line = match state.get("workers_live").and_then(|v| v.as_u64()) {
        Some(workers) => format!(" · workers {workers} live, cap {effective} of {ceiling}"),
        None => format!(" · cap {effective} of {ceiling}"),
    };
    if let Some(reason) = state.get("reason").and_then(|v| v.as_str()) {
        if !reason.is_empty() {
            line.push_str(&format!(", {reason}"));
        }
    }
    if let Some(accounts) = state.get("accounts").and_then(|v| v.as_array()) {
        for account in accounts {
            if account.get("show").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            let Some(provider) = account.get("provider").and_then(|v| v.as_str()) else {
                continue;
            };
            if provider.is_empty() {
                // A route-less record folds no tokens; printing a bare name
                // with no reading would be a gauge of nothing.
                continue;
            }
            let limit = account.get("limit_tokens").and_then(|v| v.as_u64());
            let parts: Vec<String> = account
                .get("windows")
                .and_then(|v| v.as_array())
                .map(|windows| {
                    windows
                        .iter()
                        .filter_map(|window| window_part(window, limit))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(spend) = account.get("spend_usd").and_then(|v| v.as_f64()) {
                // A metered account shows its month spend instead of windows.
                line.push_str(&format!(" · {provider} ${spend:.2} this month"));
                continue;
            }
            if parts.is_empty() {
                continue;
            }
            line.push_str(&format!(" · {provider} {}", parts.join(", ")));
        }
    }
    line
}

/// One window's readout: a percent against the account's own token limit
/// (which lives on the account, beside `windows`) when it sets one, raw
/// tokens otherwise. `weekly` prints as `wk` (the gauge convention).
fn window_part(window: &serde_json::Value, limit: Option<u64>) -> Option<String> {
    let name = window.get("window").and_then(|v| v.as_str())?;
    let label = match name {
        "weekly" => "wk",
        other => other,
    };
    let used = window.get("used_tokens").and_then(|v| v.as_u64())?;
    match limit.filter(|limit| *limit > 0) {
        Some(limit) => Some(format!(
            "{label} {:.0}%",
            used as f64 / limit as f64 * 100.0
        )),
        None => Some(format!("{label} {}", human_tokens(used))),
    }
}

/// 40000 -> "40k tok"; 1_100_000 -> "1.1M tok".
fn human_tokens(used: u64) -> String {
    let unit_value = |value: f64, unit: &str| {
        if value >= 10.0 {
            format!("{value:.0}{unit} tok")
        } else {
            format!("{value:.1}{unit} tok")
        }
    };
    if used >= 1_000_000 {
        unit_value(used as f64 / 1_000_000.0, "M")
    } else if used >= 1_000 {
        unit_value(used as f64 / 1_000.0, "k")
    } else {
        format!("{used} tok")
    }
}

#[cfg(test)]
mod tests {
    use super::{capacity_segment, human_tokens, memory_segment, parse_macmon_sample};
    use serde_json::json;

    #[test]
    fn macmon_keeps_cpu_and_watts_and_drops_the_memory_math() {
        let raw = br#"{"cpu_usage_pct":0.45,"sys_power":53.5,"memory":{"ram_total":103079215104,"ram_usage":30702266368}}"#;
        let simple = parse_macmon_sample(raw, false).unwrap();
        assert_eq!(simple, "CPU 45% busy · 54 W");
        assert_eq!(
            parse_macmon_sample(br#"{"cpu_usage_pct":0.45}"#, false).unwrap(),
            "CPU 45% busy"
        );
        let detailed = parse_macmon_sample(raw, true).unwrap();
        assert_eq!(detailed, "cpu 45% 54W");
        assert_eq!(parse_macmon_sample(b"", false), None);
    }

    #[test]
    fn the_meter_and_the_overlay_print_the_same_state_memory_figure() {
        let state = json!({
            "ceiling": 23,
            "effective": 14,
            "memory": {"used_fraction": 0.64, "total_gb": 96.0,
                       "compressor_occupied_gb": 61.4, "compressor_stored_gb": 41.0},
            "workers_live": 15,
            "accounts": [],
        });
        let memory = memory_segment(Some(&state), false);
        assert_eq!(memory, " · memory 64% full");
        // The overlay reads the same field (org_overlay::memory_line), so
        // one sample renders one percent in both places (AC5).
        let overlay_pct = (state["memory"]["used_fraction"].as_f64().unwrap() * 100.0).round();
        assert_eq!(format!("{overlay_pct:.0}% full"), "64% full");

        // No state at all: the cap reads unknown and no memory number
        // renders, the never-a-zero contract (AC6).
        assert_eq!(capacity_segment(None), " · cap unknown");
        assert_eq!(memory_segment(None, false), "");
        let bare = json!({"ceiling": 23});
        assert_eq!(capacity_segment(Some(&bare)), " · cap unknown");
        assert_eq!(memory_segment(Some(&bare), false), "");
    }

    #[test]
    fn the_capacity_line_names_workers_cap_reason_and_windows() {
        // The account shape the writer emits: limit_tokens sits on the
        // account, beside windows, never inside a window row.
        let state = json!({
            "ceiling": 23,
            "effective": 14,
            "reason": "CPU-bound",
            "workers_live": 15,
            "accounts": [
                {"provider": "zai", "show": true, "limit_tokens": 64000, "windows": [
                    {"window": "5h", "used_tokens": 40000},
                    {"window": "weekly", "used_tokens": 1100000}]},
                {"provider": "claude", "show": true, "billing": "metered",
                 "spend_usd": 12.5, "windows": []},
                {"provider": "hidden", "show": false, "windows": [
                    {"window": "5h", "used_tokens": 9}]},
            ],
        });
        let line = capacity_segment(Some(&state));
        assert!(
            line.contains("workers 15 live, cap 14 of 23, CPU-bound"),
            "{line}"
        );
        assert!(line.contains("zai 5h 63%, wk 1.1M tok"), "{line}");
        // The formatter's other shapes, pinned on the same rendered line's
        // contract: k-form tokens and a raw sub-k count.
        assert_eq!(human_tokens(40_000), "40k tok");
        assert_eq!(human_tokens(900), "900 tok");
        // A metered account shows its month spend instead of windows.
        assert!(line.contains("claude $12.50 this month"), "{line}");
        assert!(!line.contains("hidden"), "{line}");
    }
}

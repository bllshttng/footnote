//! The whole-machine resource meter: one `macmon` sample per refresh,
//! rendered as a status-row segment. Split out of `client.rs` (file budget).
//! `config.mux.load_readout` picks plain words (the default) or the raw
//! numbers; the sampler re-reads it each time the meter is switched on.

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
    parsed.unwrap_or_else(|| "meter: sensor unavailable".into())
}

pub(super) fn parse_macmon_sample(raw: &[u8], detailed: bool) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let line = text.lines().find(|l| l.trim_start().starts_with('{'))?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let cpu = value.get("cpu_usage_pct")?.as_f64()?;
    let mem = value.get("memory")?;
    let total = mem.get("ram_total")?.as_f64()?;
    let usage = mem.get("ram_usage")?.as_f64()?;
    // macmon's measured contract is a 0-1 fraction; no percent spelling to
    // rescue (the lanes arm pins the same contract).
    let cpu_pct = cpu * 100.0;
    let watts = value.get("sys_power").and_then(|p| p.as_f64());
    if !detailed && total > 0.0 {
        let mut line = format!(
            "CPU {cpu_pct:.0}% busy · memory {:.0}% full",
            usage / total * 100.0
        );
        if let Some(w) = watts {
            line.push_str(&format!(" · {w:.0} W"));
        }
        return Some(line);
    }
    let mut line = format!(
        "cpu {cpu_pct:.0}% mem {:.0}G/{:.0}G",
        usage / 1e9,
        total / 1e9
    );
    if let Some(w) = watts {
        line.push_str(&format!(" {w:.0}W"));
    }
    Some(line)
}

//! The court panel's fold leg: a bounded, fail-open call to
//! `fno doctor lanes --json`, mirroring [`crate::yard_overlay`]'s idiom.
//!
//! The operator asked to see what the machine is holding before launching
//! more lanes, and said the part that matters: "I just don't know what our
//! cap even is." The cap is `max_load_per_cpu x ncpu`, and `fno doctor
//! lanes` already knows it. This module is the SURFACE for a shipped
//! advisor, never a second capacity model: it parses one payload and renders
//! it. No arithmetic here decides anything.
//!
//! The call runs off the UI loop on a spawned task and reports back over a
//! channel, so a slow `fno` never blocks the overlay from opening.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// Ten seconds, and the number is MEASURED rather than reasoned.
///
/// `time fno doctor lanes --json` on this machine at 1-minute load 107:
/// **8.07s**. The verb's own `macmon` sample is bounded at 5.0s, so a read
/// over five seconds is its designed worst case, not a fault. A budget under
/// that guarantees a degrade exactly when the machine is busy, which is the
/// only time a person opens this panel. The plan for this change proposed
/// 2500ms on reasoning; the measurement refuted it, and the same shape (a
/// budget set against a read nobody timed) is the defect this whole node was
/// filed about.
///
/// A long budget costs nothing here. The fold runs off the UI loop, one at a
/// time, and the overlay opens on the keypress whether or not it has landed.
/// `kill_on_drop` reaps the child on the timeout, so an overrun cannot leak a
/// Python process.
const SHELLOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// The census block: kings and workers are ROW counts, tests is a PROCESS
/// count. `None` is a read that failed, and it renders as `unknown` rather
/// than as a zero: an operator who reads a fabricated zero as headroom is
/// exactly the failure the panel exists to prevent.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Census {
    #[serde(default)]
    pub kings: Option<u32>,
    #[serde(default)]
    pub king_conflicts: Option<u32>,
    #[serde(default)]
    pub workers: Option<u32>,
    #[serde(default)]
    pub tests: Option<u32>,
    #[serde(default)]
    pub roster_rows: Option<u32>,
    /// Set when live rows could not be attributed to processes. The fleet CPU
    /// share is then an UNDERCOUNT, never headroom (x-e040). It qualifies the
    /// CPU reading and is never folded into the counts above.
    #[serde(default)]
    pub attribution_gap: Option<String>,
    #[serde(default)]
    pub read_ms: Option<u64>,
}

/// One arm of the lane answer, as emitted by `fno doctor lanes --json`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Arm {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub reason: String,
}

/// One whole reading. `lane_count` is `None` on a refusal, and then
/// `refused_reason` carries the advisor's own words - which the panel
/// repeats verbatim rather than summarizing, because a dark sensor is not
/// headroom and only the advisor knows which sensor went dark.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Court {
    #[serde(default)]
    pub lane_count: Option<u32>,
    #[serde(default)]
    pub per_lane_cpu_cores: Option<f64>,
    #[serde(default)]
    pub per_lane_mem_gb: Option<f64>,
    #[serde(default)]
    pub cost_source: String,
    #[serde(default)]
    pub refused_reason: String,
    #[serde(default)]
    pub census: Census,
    #[serde(default)]
    pub arms: Vec<Arm>,
}

impl Court {
    /// The named arm, if the payload carried it.
    pub fn arm(&self, name: &str) -> Option<&Arm> {
        self.arms.iter().find(|a| a.name == name)
    }

    /// A numeric field of a measured arm's value object.
    pub fn arm_num(&self, name: &str, field: &str) -> Option<f64> {
        self.arm(name)?.value.get(field)?.as_f64()
    }

    /// A string field of a measured arm's value object.
    pub fn arm_str(&self, name: &str, field: &str) -> Option<&str> {
        self.arm(name)?.value.get(field)?.as_str()
    }
}

/// Resolve the `fno` binary through the server's one resolver, exactly as
/// the yard leg does. A second resolver with different semantics would let
/// this panel degrade on a checkout where every sibling leg still works.
fn fno_bin() -> PathBuf {
    crate::server::fno_bin()
}

/// Fold the court now. `None` on any failure (timeout, unparseable JSON) -
/// the caller shows a named degrade line.
///
/// A NONZERO exit is not a failure here: `fno doctor lanes` exits 3 on a
/// refusal and still prints the whole reading, refusal reason and census
/// included. Treating exit 3 as a dead fold would blank the panel at exactly
/// the moment its refusal is the answer.
pub async fn fold_now() -> Option<Court> {
    let mut command = crate::process_admission::tokio_command(fno_bin());
    command
        .args(["doctor", "lanes", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Dropped on timeout; kill_on_drop reaps the child so a slow fold
        // can't orphan a Python process on each overlay open.
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    parse(&output.stdout)
}

/// Parse the verb's JSON payload. Fails quiet (returns `None`) on
/// unparseable output so a torn stdout degrades the overlay, not crashes it.
fn parse(stdout: &[u8]) -> Option<Court> {
    serde_json::from_slice(stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload measured on one machine, 2026-09-04, trimmed to the
    /// fields the panel reads.
    const LIVE: &[u8] = br#"{
      "lane_count": 0,
      "per_lane_cpu_cores": 0.032,
      "per_lane_mem_gb": 0.396,
      "cost_source": "measured from the live roster's attributed footprint (50 live row(s))",
      "refused_reason": "",
      "census": {"kings": 5, "king_conflicts": 0, "workers": 45, "tests": 2,
                 "roster_rows": 50, "read_ms": 597,
                 "attribution_gap": "11 pidless row(s) with no identity route (codex)"},
      "arms": [
        {"name": "spawn load", "state": "measured",
         "value": {"load_1m": 107.3, "ceiling": 96.0, "status": "exceeded"}, "reason": ""},
        {"name": "whole-machine cpu", "state": "measured",
         "value": {"busy_fraction": 0.797, "capacity_cores": 12}, "reason": ""},
        {"name": "memory", "state": "measured",
         "value": {"free_fraction": 0.63, "available_gb": 64.9}, "reason": ""}
      ]
    }"#;

    #[test]
    fn parses_the_measured_payload() {
        let court = parse(LIVE).expect("the live payload parses");
        assert_eq!(court.lane_count, Some(0));
        assert_eq!(court.census.kings, Some(5));
        assert_eq!(court.census.workers, Some(45));
        assert_eq!(court.census.tests, Some(2));
        assert_eq!(court.census.roster_rows, Some(50));
        assert_eq!(court.census.read_ms, Some(597));
        assert_eq!(court.arm_num("spawn load", "load_1m"), Some(107.3));
        assert_eq!(court.arm_num("spawn load", "ceiling"), Some(96.0));
        assert_eq!(court.arm_str("spawn load", "status"), Some("exceeded"));
        assert_eq!(
            court.arm_num("whole-machine cpu", "capacity_cores"),
            Some(12.0)
        );
    }

    #[test]
    fn kings_plus_workers_is_the_roster() {
        // Not arithmetic the panel performs - the identity the Python census
        // guarantees by reading ONE rows list. Asserted here so a payload
        // that stops holding it is caught at the surface that renders it.
        let c = parse(LIVE).expect("parses").census;
        assert_eq!(
            c.kings.unwrap() + c.workers.unwrap(),
            c.roster_rows.unwrap()
        );
    }

    #[test]
    fn a_refusal_parses_with_its_reason_and_no_lane_number() {
        let json = br#"{"lane_count": null, "refused_reason": "the machine arms cannot answer: memory dark (macmon not on PATH)", "census": {"kings": 2, "workers": 3, "roster_rows": 5}, "arms": []}"#;
        let court = parse(json).expect("a refusal is still a reading");
        assert!(court.lane_count.is_none());
        assert!(court.refused_reason.contains("memory dark"));
        assert_eq!(court.census.kings, Some(2));
    }

    #[test]
    fn a_null_census_reads_as_unknown_not_zero() {
        // The whole point of the Option: an unreadable registry must not
        // render as a fleet of zero kings and zero workers.
        let json = br#"{"lane_count": 3, "census": {"kings": null, "workers": null, "roster_rows": null, "tests": 1}, "arms": []}"#;
        let court = parse(json).expect("parses");
        assert!(court.census.kings.is_none());
        assert!(court.census.workers.is_none());
        assert_eq!(court.census.tests, Some(1));
    }

    #[test]
    fn a_payload_with_no_census_block_still_parses() {
        // A binary newer than the fno on PATH: the census defaults to all
        // unknown rather than failing the whole fold.
        let court = parse(br#"{"lane_count": 7, "arms": []}"#).expect("parses");
        assert_eq!(court.lane_count, Some(7));
        assert_eq!(court.census, Census::default());
    }

    #[test]
    fn torn_json_fails_quiet() {
        assert!(parse(b"{not json").is_none());
        assert!(parse(b"").is_none());
    }
}

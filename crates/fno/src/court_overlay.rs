//! The court panel: what the machine is holding, and the cap that decides
//! whether another lane fits.
//!
//! The operator asked for this and said the part that matters: "I just don't
//! know what our cap even is." The cap is `max_load_per_cpu x ncpu`, and
//! `fno doctor lanes` already knows it. This module is the SURFACE for a
//! shipped advisor, never a second capacity model: it folds one payload and
//! renders it, and no arithmetic here decides anything.
//!
//! The module owns the whole panel - the bounded fold, the cached state, and
//! the render - so the client keeps only the wiring: one field, one channel,
//! one compose branch. The fold runs off the UI loop on a spawned task and
//! reports back over that channel, so a slow `fno` never blocks the overlay
//! from opening.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Ten seconds, and the number is MEASURED rather than reasoned.
///
/// `time fno doctor lanes --json` on this machine at 1-minute load 107:
/// **8.07s**. The verb's own `macmon` sample is bounded at 5.0s, so a read
/// over five seconds is its designed worst case, not a fault. A budget under
/// that guarantees a degrade exactly when the machine is busy, which is the
/// only time a person opens this panel. The plan for this change proposed
/// 2500ms on reasoning, and it failed against the live verb on the first
/// run. Setting a budget against a read nobody timed is the defect this
/// whole change was filed about.
///
/// A long budget costs nothing here. The fold runs off the UI loop, one at a
/// time, and the overlay opens on the keypress whether or not it has landed.
/// `kill_on_drop` reaps the child on the timeout, so an overrun cannot leak
/// a Python process.
const SHELLOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Fifteen seconds, not the yard's sixty. The yard renders identity, which
/// does not move; this renders load, which does.
///
/// The floor is the same measurement: a TTL under the read's own 8.07s cost
/// means every open refetches and every reading is already stale on arrival,
/// which is a busy loop wearing a cache's clothes. A reading older than this
/// still RENDERS, with its age and the word `stale`, never as `unknown`. An
/// operator watching `unknown` every second learns nothing and reaches for
/// `--force`, which is how a guard becomes a formality.
pub const CACHE_TTL: Duration = Duration::from_secs(15);

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

/// One count, rendered so an unread number says so. `unknown` is printed
/// ONLY where the read genuinely failed; a fabricated zero read as headroom
/// is the failure this panel exists to prevent.
fn count_or_unknown(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_string(), |v| v.to_string())
}

/// The panel's whole state. It lives across closes on purpose: the cached
/// reading and the fold generation both have to outlive the overlay, or a
/// reopen inside the TTL would pay the slow read again and a result landing
/// after a close would merge into the wrong generation.
#[derive(Debug, Default)]
pub struct Panel {
    open: bool,
    fold: Option<Court>,
    fold_at: Option<Instant>,
    degraded: bool,
    want: bool,
    inflight: bool,
    gen: u64,
}

impl Panel {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the panel. It ALWAYS opens: a fold that is pending, stale or
    /// failed renders its own line, never a blank panel. Inside the TTL the
    /// cached reading is served with its age shown, which is what keeps a
    /// reopen off the slow read. A stale reading is KEPT rather than
    /// dropped, so the panel can render it as stale while the refresh runs;
    /// dropping it here is what would put `unknown` on screen.
    ///
    /// `degraded` is NOT cleared here. Only a fold that actually succeeds
    /// clears it. Clearing on open erases a failed refresh the moment the
    /// operator looks away and back, and inside the TTL no new fold runs to
    /// re-discover it, so the panel would quietly present a reading whose
    /// last refresh failed as if nothing had gone wrong.
    pub fn open(&mut self) {
        self.open = true;
        self.gen = self.gen.wrapping_add(1);
        if !self.fold_at.is_some_and(|t| t.elapsed() < CACHE_TTL) {
            self.want = true;
        }
    }

    pub fn close(&mut self) {
        if self.open {
            self.open = false;
            self.gen = self.gen.wrapping_add(1);
        }
    }

    /// The generation to spawn a fold under, or `None` when nothing is
    /// wanted or one is already in flight. Single-flight by construction, so
    /// holding the key down cannot queue a pile of Python processes.
    pub fn take_want(&mut self) -> Option<u64> {
        if !self.want || self.inflight {
            return None;
        }
        self.want = false;
        self.inflight = true;
        Some(self.gen)
    }

    /// Merge a landed fold. Returns whether it merged: a result for a closed
    /// or superseded generation is discarded, and a FAILED fold never stamps
    /// `fold_at`, so the age line keeps describing the last real reading
    /// rather than the moment the failure arrived.
    pub fn apply(&mut self, gen: u64, result: Option<Court>) -> bool {
        self.inflight = false;
        if gen != self.gen || !self.open {
            return false;
        }
        match result {
            Some(court) => {
                self.fold = Some(court);
                self.degraded = false;
                self.fold_at = Some(Instant::now());
            }
            None => self.degraded = true,
        }
        true
    }

    /// The panel's lines: load against the cap, the machine, the census, the
    /// lane advisor's own answer, and the age of the reading.
    ///
    /// Three rules this render keeps, each closing a way a monitor can lie.
    /// The `read` line always shows the fold's age and says `stale` rather
    /// than `unknown`. The attribution gap gets its own line and is never
    /// folded into a count. A refusal prints the advisor's own words
    /// verbatim with no lane number beside them.
    pub fn lines(&self) -> Vec<String> {
        let Some(court) = self.fold.as_ref() else {
            return vec![if self.degraded {
                format!(
                    "  fold failed - `fno doctor lanes` did not answer inside {}s",
                    SHELLOUT_TIMEOUT.as_secs()
                )
            } else {
                "  reading the machine...".to_string()
            }];
        };
        let mut lines = Vec::new();

        // load against the cap: the operator's own question, first.
        match (
            court.arm_num("spawn load", "load_1m"),
            court.arm_num("spawn load", "ceiling"),
        ) {
            (Some(load), Some(ceiling)) => {
                let status = court.arm_str("spawn load", "status").unwrap_or("unknown");
                lines.push(format!("  load    {load:.1} / {ceiling:.1}   {status}"));
            }
            _ => {
                let reason = court
                    .arm("spawn load")
                    .map_or("arm absent", |a| a.reason.as_str());
                lines.push(format!("  load    unknown - {reason}"));
            }
        }

        // the machine the lanes compete for, not the fleet alone.
        match (
            court.arm_num("whole-machine cpu", "busy_fraction"),
            court.arm_num("whole-machine cpu", "capacity_cores"),
        ) {
            (Some(busy), Some(cores)) => lines.push(format!(
                "  cpu     {:.0}% busy of {cores:.0} cores",
                busy * 100.0
            )),
            _ => lines.push("  cpu     unknown".to_string()),
        }
        if let Some(free) = court.arm_num("memory", "available_gb") {
            lines.push(format!("  memory  {free:.1} GB available"));
        }

        // the court itself.
        let census = &court.census;
        lines.push(format!(
            "  court   {} kings · {} workers · {} tests ({} rows)",
            count_or_unknown(census.kings),
            count_or_unknown(census.workers),
            count_or_unknown(census.tests),
            count_or_unknown(census.roster_rows),
        ));
        if let Some(conflicts) = census.king_conflicts.filter(|c| *c > 0) {
            lines.push(format!(
                "  warn    {conflicts} scope(s) held by more than one crown"
            ));
        }

        // the advisor's answer, or the advisor's refusal in its own words.
        match court.lane_count {
            Some(count) => {
                let cost = match (court.per_lane_cpu_cores, court.per_lane_mem_gb) {
                    (Some(c), Some(g)) => format!(" · {c:.3} cores, {g:.2} GB per lane"),
                    _ => String::new(),
                };
                lines.push(format!("  lanes   {count} more fit{cost}"));
                if !court.cost_source.is_empty() {
                    lines.push(format!("          {}", court.cost_source));
                }
            }
            None => {
                let reason = if court.refused_reason.is_empty() {
                    "no lane number: the advisor refused without naming a reason"
                } else {
                    court.refused_reason.as_str()
                };
                lines.push(format!("  lanes   REFUSED - {reason}"));
            }
        }

        if let Some(gap) = census.attribution_gap.as_ref() {
            lines.push(format!("  gap     {gap}"));
            lines.push("          fleet share is an undercount, not headroom".to_string());
        }

        let age = self.fold_at.map_or_else(
            || "age unknown".to_string(),
            |t| {
                let secs = t.elapsed().as_secs_f32();
                if t.elapsed() > CACHE_TTL {
                    format!("{secs:.1}s ago (stale, refreshing)")
                } else {
                    format!("{secs:.1}s ago")
                }
            },
        );
        let degraded = if self.degraded {
            " · last refresh failed"
        } else {
            ""
        };
        lines.push(format!("  read    {age}{degraded}"));
        lines
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
                 "roster_rows": 50, "read_ms": 597, "attribution_gap": null},
      "arms": [
        {"name": "spawn load", "state": "measured",
         "value": {"load_1m": 107.3, "ceiling": 96.0, "status": "exceeded"}, "reason": ""},
        {"name": "whole-machine cpu", "state": "measured",
         "value": {"busy_fraction": 0.797, "capacity_cores": 12}, "reason": ""},
        {"name": "memory", "state": "measured",
         "value": {"free_fraction": 0.63, "available_gb": 64.9}, "reason": ""}
      ]
    }"#;

    fn live() -> Court {
        parse(LIVE).expect("the measured payload parses")
    }

    /// A panel holding one landed reading, opened.
    fn opened(court: Court) -> Panel {
        let mut panel = Panel::default();
        panel.open();
        let gen = panel.take_want().expect("a fresh panel wants a fold");
        assert!(panel.apply(gen, Some(court)));
        panel
    }

    #[test]
    fn parses_the_measured_payload() {
        let court = live();
        assert_eq!(court.lane_count, Some(0));
        assert_eq!(court.census.kings, Some(5));
        assert_eq!(court.census.workers, Some(45));
        assert_eq!(court.census.tests, Some(2));
        assert_eq!(court.census.read_ms, Some(597));
        assert_eq!(court.arm_num("spawn load", "load_1m"), Some(107.3));
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
        let c = live().census;
        assert_eq!(
            c.kings.unwrap() + c.workers.unwrap(),
            c.roster_rows.unwrap()
        );
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

    #[test]
    fn ac4_hp_renders_load_census_lanes_and_an_age() {
        let text = opened(live()).lines().join("\n");

        assert!(text.contains("load    107.3 / 96.0"), "{text}");
        assert!(text.contains("exceeded"), "{text}");
        assert!(text.contains("cpu     80% busy of 12 cores"), "{text}");
        assert!(text.contains("memory  64.9 GB available"), "{text}");
        assert!(
            text.contains("court   5 kings · 45 workers · 2 tests (50 rows)"),
            "{text}"
        );
        assert!(text.contains("lanes   0 more fit"), "{text}");
        assert!(text.contains("0.032 cores"), "{text}");
        assert!(text.contains("read    0.0s ago"), "{text}");
    }

    #[test]
    fn ac4_edge_a_failed_fold_opens_with_a_named_degrade_never_a_blank_panel() {
        let mut panel = Panel::default();
        panel.open();
        let gen = panel.take_want().expect("wants a fold");
        assert!(panel.apply(gen, None));

        let lines = panel.lines();

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("fold failed"), "{lines:?}");
        assert!(lines[0].contains("10s"), "{lines:?}");
    }

    #[test]
    fn a_pending_fold_says_so_rather_than_showing_zeroes() {
        let mut panel = Panel::default();
        panel.open();

        let lines = panel.lines();

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("reading the machine"), "{lines:?}");
        assert!(
            !lines[0].contains('0'),
            "a pending fold must show no counts"
        );
    }

    #[test]
    fn ac4_edge2_a_refusal_carries_the_advisors_words_and_no_lane_number() {
        let mut court = live();
        court.lane_count = None;
        court.refused_reason =
            "the machine arms cannot answer the lane question: memory dark (macmon not on PATH)"
                .to_string();
        let text = opened(court).lines().join("\n");

        assert!(text.contains("lanes   REFUSED"), "{text}");
        assert!(text.contains("memory dark (macmon not on PATH)"), "{text}");
        assert!(!text.contains("more fit"), "{text}");
    }

    #[test]
    fn a_refusal_with_no_reason_still_says_no_number_rather_than_printing_one() {
        let mut court = live();
        court.lane_count = None;
        court.refused_reason = String::new();
        let text = opened(court).lines().join("\n");

        assert!(text.contains("REFUSED"), "{text}");
        assert!(text.contains("without naming a reason"), "{text}");
    }

    #[test]
    fn the_attribution_gap_gets_its_own_line_and_never_a_count() {
        // x-e040 made the gap honest. A panel that folded it into a count
        // would re-open the hole: the gap is a process-to-row failure and
        // cannot change how many rows exist.
        let mut court = live();
        court.census.attribution_gap =
            Some("11 pidless row(s) with no identity route (codex)".to_string());
        let text = opened(court).lines().join("\n");

        assert!(text.contains("gap     11 pidless row(s)"), "{text}");
        assert!(text.contains("undercount, not headroom"), "{text}");
        assert!(text.contains("5 kings · 45 workers"), "{text}");
    }

    #[test]
    fn an_unreadable_registry_renders_unknown_never_a_fabricated_zero() {
        let mut court = live();
        court.census = Census {
            tests: Some(3),
            ..Census::default()
        };
        let text = opened(court).lines().join("\n");

        assert!(
            text.contains("court   unknown kings · unknown workers · 3 tests (unknown rows)"),
            "{text}"
        );
    }

    #[test]
    fn a_king_conflict_is_warned_because_a_bare_count_hides_it() {
        let mut court = live();
        court.census.king_conflicts = Some(2);

        let text = opened(court).lines().join("\n");

        assert!(
            text.contains("warn    2 scope(s) held by more than one crown"),
            "{text}"
        );
    }

    #[test]
    fn a_failed_refresh_over_a_good_reading_keeps_the_numbers_and_says_so() {
        let mut panel = opened(live());
        panel.want = true;
        let gen = panel.take_want().expect("wants a refresh");
        assert!(panel.apply(gen, None));

        let text = panel.lines().join("\n");

        assert!(text.contains("last refresh failed"), "{text}");
        assert!(text.contains("court   5 kings"), "{text}");
        // The failed refresh did NOT restamp the age: the line still
        // describes the last real reading.
        assert!(text.contains("read    0.0s ago"), "{text}");
    }

    #[test]
    fn a_stale_reading_says_stale_and_still_shows_its_numbers() {
        // The failure this closes: an operator watching "unknown" every
        // second learns nothing and reaches for --force.
        let mut panel = opened(live());
        panel.fold_at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(2));

        let text = panel.lines().join("\n");

        assert!(text.contains("stale, refreshing"), "{text}");
        assert!(text.contains("court   5 kings"), "{text}");
        assert!(!text.contains("unknown"), "{text}");
    }

    #[test]
    fn a_dark_load_arm_names_its_reason_rather_than_printing_a_number() {
        let court = parse(
            br#"{"lane_count": null, "refused_reason": "arms dark",
                 "census": {}, "arms": [{"name": "spawn load", "state": "dark",
                 "value": null, "reason": "load average unreadable"}]}"#,
        )
        .expect("parses");

        let text = opened(court).lines().join("\n");

        assert!(
            text.contains("load    unknown - load average unreadable"),
            "{text}"
        );
    }

    #[test]
    fn a_failed_refresh_survives_a_close_and_reopen() {
        // Clearing `degraded` on open would erase the failure the moment the
        // operator looks away and back, and inside the TTL no new fold runs
        // to re-discover it.
        let mut panel = opened(live());
        panel.want = true;
        let gen = panel.take_want().expect("wants a refresh");
        assert!(panel.apply(gen, None));
        panel.close();
        panel.open();

        assert!(panel.take_want().is_none(), "still inside the TTL");
        assert!(
            panel.lines().join("\n").contains("last refresh failed"),
            "the reopen must not hide the failed refresh"
        );
    }

    #[test]
    fn a_successful_refresh_clears_the_failure_note() {
        let mut panel = opened(live());
        panel.want = true;
        let gen = panel.take_want().expect("wants a refresh");
        assert!(panel.apply(gen, None));
        panel.want = true;
        let gen = panel.take_want().expect("wants another");
        assert!(panel.apply(gen, Some(live())));

        assert!(!panel.lines().join("\n").contains("last refresh failed"));
    }

    #[test]
    fn a_reopen_inside_the_ttl_serves_the_cache_and_spawns_no_fold() {
        let mut panel = opened(live());
        panel.close();
        panel.open();

        assert!(panel.take_want().is_none(), "no refetch inside the TTL");
        assert!(panel.lines().join("\n").contains("court   5 kings"));
    }

    #[test]
    fn a_reopen_past_the_ttl_refetches_and_keeps_showing_the_stale_reading() {
        let mut panel = opened(live());
        panel.fold_at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        panel.close();
        panel.open();

        assert!(panel.take_want().is_some(), "a stale reading refetches");
        let text = panel.lines().join("\n");
        assert!(text.contains("stale, refreshing"), "{text}");
        assert!(text.contains("5 kings"), "{text}");
    }

    #[test]
    fn a_fold_landing_after_a_close_is_discarded() {
        // The generation guard: a result for a superseded open must not
        // repaint a panel nobody is looking at.
        let mut panel = Panel::default();
        panel.open();
        let gen = panel.take_want().expect("wants a fold");
        panel.close();

        assert!(!panel.apply(gen, Some(live())));
        assert!(panel.fold.is_none());
    }

    #[test]
    fn only_one_fold_is_ever_in_flight() {
        let mut panel = Panel::default();
        panel.open();

        assert!(panel.take_want().is_some());
        assert!(
            panel.take_want().is_none(),
            "holding the key down must not queue Python processes"
        );
    }

    #[test]
    fn the_court_binding_is_c_in_global_and_dispatches_open_court() {
        let binding = crate::keys::key_bindings()
            .into_iter()
            .find(|b| b.action == "court")
            .expect("the court action is bound");

        assert_eq!(binding.key, b'C');
        assert_eq!(binding.event, crate::keys::Event::OpenCourt);
        assert!(matches!(binding.section, crate::keys::KeySection::Global));
    }
}

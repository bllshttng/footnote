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

/// The minimized block is exactly three glance lines: load, cpu, census.
/// The client reserves this many sideline rows for it, so the height the
/// layout subtracts and the height the painter draws cannot drift.
pub const MINIMIZED_ROWS: usize = 3;

/// The census block: kings are a ROW count, tests is a PROCESS count. `None`
/// is a read that failed, and it renders as `unknown` rather than as a zero:
/// an operator who reads a fabricated zero as headroom is exactly the
/// failure the panel exists to prevent. The worker total is NOT here - the
/// panel derives it from the row ages it already holds, so one source
/// answers "how many are working".
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
    /// The caller's own share reading, from the one function the spawn gate
    /// refuses on (x-5283 AC3). `None` when the census could not read it.
    #[serde(default)]
    pub share: Option<ShareReading>,
    /// Top fleet consumers by program name, aggregated from the ps read the
    /// lanes fold already performs. `None` when the footprint went dark.
    #[serde(default)]
    pub top_consumers: Option<Vec<TopConsumer>>,
    #[serde(default)]
    pub read_ms: Option<u64>,
}

/// The caller's own share reading (x-5283), produced by the same Python
/// function the spawn gate refuses on. `None` counts are a failed read and
/// render `unknown`, never zero.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ShareReading {
    #[serde(default)]
    pub kings: Option<u32>,
    #[serde(default)]
    pub share: Option<u32>,
    #[serde(default)]
    pub held: Option<u32>,
    #[serde(default)]
    pub unattributed: Option<Unattributed>,
}

/// The live rows that name nobody (x-5283 LD4): one named bucket, count plus
/// row names. They divide nothing and pay no king's tax.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Unattributed {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub rows: Vec<String>,
}

/// One program in the expanded panel's `top` block: name, process count,
/// summed ps `%cpu`, and the worktree the most of its rows run from.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TopConsumer {
    pub name: String,
    #[serde(default)]
    pub procs: u32,
    #[serde(default)]
    pub cpu_pct: f64,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub worktree_procs: u32,
}

/// The activity census, bucketed from the transcript-derived age the daemon
/// already stamps on every row it hands the client. Under five minutes is
/// working; five minutes to two hours idle; two to eight hours stale; past
/// eight hours dead, the reap backlog. A row with no age is its own bucket
/// and is never folded into a live one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CensusSplit {
    pub working: u32,
    pub idle: u32,
    pub stale: u32,
    pub dead: u32,
    pub unknown_age: u32,
}

const WORKING_MAX_S: u64 = 5 * 60;
const IDLE_MAX_S: u64 = 2 * 3600;
const STALE_MAX_S: u64 = 8 * 3600;

pub fn census_split(ages: &[Option<u64>]) -> CensusSplit {
    let mut split = CensusSplit::default();
    for age in ages {
        match age {
            None => split.unknown_age += 1,
            Some(s) if *s < WORKING_MAX_S => split.working += 1,
            Some(s) if *s < IDLE_MAX_S => split.idle += 1,
            Some(s) if *s < STALE_MAX_S => split.stale += 1,
            Some(_) => split.dead += 1,
        }
    }
    split
}

impl CensusSplit {
    /// The buckets that hold someone, joined for render. A zero bucket is
    /// omitted: on an all-unknown roster nothing may read as live headroom.
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (count, label) in [
            (self.working, "working"),
            (self.idle, "idle"),
            (self.stale, "stale"),
            (self.dead, "dead"),
            (self.unknown_age, "unknown age"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        parts.join(" · ")
    }
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

/// The panel's whole state. The block is painted on every frame now, so
/// there is no open/close: `expanded` is the only operator-facing state, and
/// the refresh cadence is independent of it - a minimized block still wants
/// a fresh reading, and an expand never spawns a fold of its own.
#[derive(Debug, Default)]
pub struct Panel {
    expanded: bool,
    fold: Option<Court>,
    fold_at: Option<Instant>,
    /// A failed fold retries one TTL out, not on the next loop pass: with
    /// the block painted every frame, an instantly-failing fold (a missing
    /// `fno` on PATH) would otherwise become a hot refetch loop.
    retry_at: Option<Instant>,
    degraded: bool,
    inflight: bool,
}

impl Panel {
    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Expand in place, or collapse back to the three-line glance. A pure
    /// render toggle: the cached reading survives, so no expand pays the
    /// slow fold again.
    pub fn toggle(&mut self) {
        self.expanded = !self.expanded;
    }

    /// The timer wake for the next refresh, or `None` while a fold is
    /// already running: a past-due deadline would re-fire every pass and the
    /// single-flight refusal would just burn loop passes until it lands.
    /// `now` when no reading has ever landed (the block is visible from the
    /// first frame, so the first fold must not wait for an event), the retry
    /// backoff after a failure, else the last landing plus the TTL.
    pub fn refresh_deadline(&self) -> Option<Instant> {
        if self.inflight {
            return None;
        }
        // The retry backoff WINS over a stale reading's due time: otherwise a
        // failed refresh over an already-stale reading leaves a past-due
        // deadline, and the timer branch busy-spins until the backoff ends.
        let due = match self.fold_at {
            Some(t) => t + CACHE_TTL,
            None => Instant::now(),
        };
        Some(self.retry_at.map_or(due, |r| r.max(due)))
    }

    /// Arm and consume the refresh want: true exactly when a fold should
    /// spawn now - none in flight, no live reading inside the TTL, and no
    /// fresh failure in its retry backoff. Single-flight by construction, so
    /// a permanently visible block can never mean a permanently folding one,
    /// and a missing `fno` cannot become a hot refetch loop.
    pub fn take_want(&mut self) -> bool {
        let now = Instant::now();
        if self.inflight
            || self
                .fold_at
                .is_some_and(|t| now.duration_since(t) < CACHE_TTL)
            || self.retry_at.is_some_and(|t| now < t)
        {
            return false;
        }
        self.inflight = true;
        true
    }

    /// Merge a landed fold. A FAILED fold never stamps `fold_at`, so the age
    /// line keeps describing the last real reading rather than the moment
    /// the failure arrived, and the failure arms a retry one TTL out instead
    /// of letting the want re-arm instantly into a hot loop.
    pub fn apply(&mut self, result: Option<Court>) {
        self.inflight = false;
        match result {
            Some(court) => {
                self.fold = Some(court);
                self.degraded = false;
                self.retry_at = None;
                self.fold_at = Some(Instant::now());
            }
            None => {
                self.degraded = true;
                self.retry_at = Some(Instant::now() + CACHE_TTL);
            }
        }
    }

    /// The minimized block: exactly three lines, painted at the bottom of
    /// the sideline on every frame. Load, cpu, census - the glance answers.
    /// Every number carries its unit or its comparand here too; the expanded
    /// view adds the detail.
    pub fn minimized_lines(&self, ages: &[Option<u64>]) -> Vec<String> {
        let Some(court) = self.fold.as_ref() else {
            let first = if self.degraded {
                "  court     fold failed - retrying".to_string()
            } else {
                "  court     reading the machine...".to_string()
            };
            return vec![first, String::new(), String::new()];
        };
        let load_line = match (
            court.arm_num("spawn load", "load_1m"),
            court.arm_num("spawn load", "ceiling"),
        ) {
            (Some(load), Some(ceiling)) => {
                let mut l = format!("  load      {load:.1} of {ceiling:.1} max");
                if load > ceiling {
                    // x-5283: the over verdict names the axis it read, so a
                    // high load average is never mistaken for the cpu line's
                    // sustained reading.
                    l.push_str(&format!(" · {:.1}x over on load_1m", load / ceiling));
                }
                l
            }
            _ => format!(
                "  load      unknown - {}",
                court
                    .arm("spawn load")
                    .map_or("arm absent", |a| a.reason.as_str())
            ),
        };
        let cpu_line = match (
            court.arm_num("whole-machine cpu", "busy_fraction"),
            court.arm_num("whole-machine cpu", "capacity_cores"),
        ) {
            (Some(busy), Some(cores)) => {
                format!("  cpu       {:.0}% busy of {cores:.0} cores", busy * 100.0)
            }
            _ => "  cpu       unknown".to_string(),
        };
        let census_line = if ages.is_empty() {
            "  workers   no roster rows held".to_string()
        } else {
            format!(
                "  workers   {} ({} rows)",
                census_split(ages).render(),
                ages.len()
            )
        };
        vec![load_line, cpu_line, census_line]
    }

    /// The expanded block: the full reading, in place. Three rules this
    /// render keeps, each closing a way a monitor can lie. The `read` line
    /// always shows the fold's age and says `stale` rather than `unknown`.
    /// The attribution gap gets its own line and is never folded into a
    /// count. A refusal prints the advisor's own words verbatim with no lane
    /// number beside them.
    pub fn expanded_lines(&self, ages: &[Option<u64>]) -> Vec<String> {
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

        // load against the cap: the operator's own question, first. The
        // 1m/5m/15m trio is what tells a climb from a spike; the factored
        // ceiling is what turns 96.0 into 8.0 per cpu x 12 cores; and the
        // comparand sentence is what turns 184.9 into 1.9x over.
        match (
            court.arm_num("spawn load", "load_1m"),
            court.arm_num("spawn load", "ceiling"),
        ) {
            (Some(load), Some(ceiling)) => {
                let mut first = format!("  {label:<8} {load:.1} now", label = "load avg");
                if let (Some(l5), Some(l15)) = (
                    court.arm_num("spawn load", "load_5m"),
                    court.arm_num("spawn load", "load_15m"),
                ) {
                    first.push_str(&format!(" · {l5:.1} 5m · {l15:.1} 15m"));
                }
                lines.push(first);
                let mut second = format!("            ceiling {ceiling:.1}");
                if let (Some(per_cpu), Some(ncpu)) = (
                    court.arm_num("spawn load", "max_load_per_cpu"),
                    court.arm_num("spawn load", "load_cpu_count"),
                ) {
                    second.push_str(&format!(" = {per_cpu:.1} per cpu x {ncpu:.0} cores"));
                }
                if load > ceiling {
                    second.push_str(&format!(" · {:.1}x over on load_1m", load / ceiling));
                }
                lines.push(second);
                lines.push("            load counts QUEUED threads, not busy time".to_string());
            }
            _ => {
                let reason = court
                    .arm("spawn load")
                    .map_or("arm absent", |a| a.reason.as_str());
                lines.push(format!(
                    "  {label:<8} unknown - {reason}",
                    label = "load avg"
                ));
            }
        }

        // the machine the lanes compete for, not the fleet alone. The 100%
        // reading was TRUE and unbelievable, so a saturated box names the
        // place the explanation lives.
        match (
            court.arm_num("whole-machine cpu", "busy_fraction"),
            court.arm_num("whole-machine cpu", "capacity_cores"),
        ) {
            (Some(busy), Some(cores)) => {
                let saturated = if busy >= 0.995 {
                    format!(" (all {cores:.0} saturated; see top)")
                } else {
                    String::new()
                };
                lines.push(format!(
                    "  {label:<8} {:.0}% busy of {cores:.0} cores{saturated}",
                    busy * 100.0,
                    label = "cpu"
                ));
            }
            _ => lines.push(format!("  {label:<8} unknown", label = "cpu")),
        }
        if let Some(free) = court.arm_num("memory", "available_gb") {
            lines.push(format!(
                "  {label:<8} {free:.1} GB available",
                label = "memory"
            ));
        }

        // the census, split by what the operator actually asked: how many
        // are working. The worker total is the sum of the buckets, from the
        // one source the client holds; the lanes census keeps only what this
        // panel cannot see.
        if ages.is_empty() {
            lines.push(format!(
                "  {label:<8} no roster rows held",
                label = "workers"
            ));
        } else {
            let split = census_split(ages);
            lines.push(format!(
                "  {label:<8} {} ({} rows)",
                split.render(),
                ages.len(),
                label = "workers"
            ));
            if split.dead > 0 {
                lines.push(format!(
                    "            {} dead: run `fno agents reap --apply`",
                    split.dead
                ));
            }
        }
        let census = &court.census;
        lines.push(format!(
            "  {label:<8} {} kings · {} test processes",
            count_or_unknown(census.kings),
            count_or_unknown(census.tests),
            label = "court"
        ));
        if let Some(conflicts) = census.king_conflicts.filter(|c| *c > 0) {
            lines.push(format!(
                "  {label:<8} {conflicts} scope(s) held by more than one crown",
                label = "warn"
            ));
        }
        // x-5283: the caller's own share, from the one function the spawn
        // gate refuses on. The unattributed bucket renders only when it
        // holds someone - a count that sees nobody says so by being absent.
        if let Some(share) = &census.share {
            let mut line = format!(
                "  {label:<8} held {} of share {} across {} kings",
                count_or_unknown(share.held),
                count_or_unknown(share.share),
                count_or_unknown(share.kings),
                label = "share"
            );
            if let Some(unattributed) = share.unattributed.as_ref().filter(|u| u.count > 0) {
                let names: Vec<&str> = unattributed.rows.iter().map(String::as_str).collect();
                let shown = if names.len() > 3 {
                    format!("{}…", names[..3].join(", "))
                } else {
                    names.join(", ")
                };
                line.push_str(&format!(" · {} unattributed ({shown})", unattributed.count));
            }
            lines.push(line);
        }

        // what is saturating the box, from the ps read the fold already
        // performed - never a second instrument.
        match census.top_consumers.as_ref().map(Vec::as_slice) {
            Some([]) => lines.push(format!(
                "  {label:<8} no fleet processes measured",
                label = "top"
            )),
            Some(consumers) => {
                let items = consumers
                    .iter()
                    .map(|c| format!("{} {} procs {:.0}%", c.name, c.procs, c.cpu_pct))
                    .collect::<Vec<_>>()
                    .join(" · ");
                lines.push(format!("  {label:<8} {items}", label = "top"));
                for c in consumers
                    .iter()
                    .filter(|c| c.worktree_procs >= 2 && c.worktree.is_some())
                {
                    lines.push(format!(
                        "            {} of {} in {}",
                        c.worktree_procs,
                        c.name,
                        c.worktree.as_deref().unwrap_or_default()
                    ));
                }
            }
            None => lines.push(format!(
                "  {label:<8} unavailable - the fleet footprint reading is dark",
                label = "top"
            )),
        }

        // the advisor's answer, or the advisor's refusal in its own words.
        match court.lane_count {
            Some(count) => {
                let cost = match (court.per_lane_cpu_cores, court.per_lane_mem_gb) {
                    (Some(c), Some(g)) => format!(" · {c:.3} cores, {g:.2} GB per lane"),
                    _ => String::new(),
                };
                lines.push(format!(
                    "  {label:<8} {count} more fit{cost}",
                    label = "lanes"
                ));
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
                lines.push(format!("  {label:<8} REFUSED - {reason}", label = "lanes"));
            }
        }

        if let Some(gap) = census.attribution_gap.as_ref() {
            lines.push(format!("  {label:<8} {gap}", label = "gap"));
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
        lines.push(format!("  {label:<8} {age}{degraded}", label = "read"));
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
    /// fields the panel reads. The `workers` count stays in the fixture to
    /// prove the render never reads it: the split is the worker answer now.
    const LIVE: &[u8] = br#"{
      "lane_count": 0,
      "per_lane_cpu_cores": 0.032,
      "per_lane_mem_gb": 0.396,
      "cost_source": "measured from the live roster's attributed footprint (50 live row(s))",
      "refused_reason": "",
      "census": {"kings": 5, "king_conflicts": 0, "workers": 45, "tests": 2,
                 "roster_rows": 50, "read_ms": 597, "attribution_gap": null,
                 "share": {"kings": 4, "share": 7, "held": 2,
                           "unattributed": {"count": 2, "rows": ["ghost-a", "ghost-b"]}},
                 "top_consumers": [
                   {"name": "fno-py", "procs": 23, "cpu_pct": 41.2,
                    "worktree": ".fno/worktrees/x-b1ee", "worktree_procs": 22},
                   {"name": "fno-agents-worker", "procs": 18, "cpu_pct": 27.9,
                    "worktree": ".fno/worktrees/x-b1ee", "worktree_procs": 18}
                 ]},
      "arms": [
        {"name": "spawn load", "state": "measured",
         "value": {"load_1m": 107.3, "load_5m": 99.5, "load_15m": 88.2,
                   "ceiling": 96.0, "max_load_per_cpu": 8.0, "load_cpu_count": 12,
                   "status": "exceeded"}, "reason": ""},
        {"name": "whole-machine cpu", "state": "measured",
         "value": {"busy_fraction": 0.797, "capacity_cores": 12}, "reason": ""},
        {"name": "memory", "state": "measured",
         "value": {"free_fraction": 0.63, "available_gb": 64.9}, "reason": ""}
      ]
    }"#;

    fn live() -> Court {
        parse(LIVE).expect("the measured payload parses")
    }

    /// The AC6 roster: one row per bucket plus one with no age.
    const AC6_AGES: [Option<u64>; 7] = [
        Some(10),
        Some(200),
        Some(900),
        Some(4000),
        Some(20_000),
        Some(40_000),
        None,
    ];

    /// A panel holding one landed reading.
    fn opened(court: Court) -> Panel {
        let mut panel = Panel::default();
        assert!(panel.take_want(), "a fresh panel wants a fold");
        panel.apply(Some(court));
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
        assert_eq!(court.arm_num("spawn load", "load_5m"), Some(99.5));
        assert_eq!(court.arm_num("spawn load", "load_15m"), Some(88.2));
        assert_eq!(court.arm_num("spawn load", "max_load_per_cpu"), Some(8.0));
        assert_eq!(court.arm_num("spawn load", "load_cpu_count"), Some(12.0));
        assert_eq!(court.arm_str("spawn load", "status"), Some("exceeded"));
        assert_eq!(
            court.arm_num("whole-machine cpu", "capacity_cores"),
            Some(12.0)
        );
        let top = court
            .census
            .top_consumers
            .as_ref()
            .expect("top consumers parse");
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "fno-py");
        assert_eq!(top[0].worktree.as_deref(), Some(".fno/worktrees/x-b1ee"));
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
    fn ac5_hp_the_share_line_names_held_share_and_the_unattributed_bucket() {
        let text = opened(live()).expanded_lines(&AC6_AGES).join("\n");

        assert!(
            text.contains("share    held 2 of share 7 across 4 kings"),
            "{text}"
        );
        assert!(text.contains("2 unattributed (ghost-a, ghost-b)"), "{text}");
    }

    #[test]
    fn an_empty_unattributed_bucket_renders_nothing() {
        // Nobody in the bucket: the line stays clean rather than printing a
        // zero that reads as a fact about liveness.
        let mut court = live();
        if let Some(share) = court.census.share.as_mut() {
            share.unattributed = None;
        }
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("share    held 2 of share 7"), "{text}");
        assert!(!text.contains("unattributed"), "{text}");
    }

    #[test]
    fn torn_json_fails_quiet() {
        assert!(parse(b"{not json").is_none());
        assert!(parse(b"").is_none());
    }

    #[test]
    fn a_refusal_with_no_reason_still_says_no_number_rather_than_printing_one() {
        let mut court = live();
        court.lane_count = None;
        court.refused_reason = String::new();
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("REFUSED"), "{text}");
        assert!(text.contains("without naming a reason"), "{text}");
    }

    #[test]
    fn census_split_buckets_by_activity_age() {
        let split = census_split(&AC6_AGES);
        assert_eq!(split.working, 2); // 10s, 200s: under 5m
        assert_eq!(split.idle, 2); // 900s, 4000s: 15m, 66m
        assert_eq!(split.stale, 1); // 20000s: 5.6h
        assert_eq!(split.dead, 1); // 40000s: 11h, the reap backlog
        assert_eq!(split.unknown_age, 1);
    }

    #[test]
    fn ac4_hp_every_load_number_names_its_unit_and_comparand() {
        let text = opened(live()).expanded_lines(&AC6_AGES).join("\n");

        assert!(
            text.contains("load avg 107.3 now · 99.5 5m · 88.2 15m"),
            "{text}"
        );
        assert!(
            text.contains("ceiling 96.0 = 8.0 per cpu x 12 cores"),
            "{text}"
        );
        assert!(text.contains("1.1x over on load_1m"), "{text}");
        assert!(
            text.contains("load counts QUEUED threads, not busy time"),
            "{text}"
        );
        assert!(text.contains("80% busy of 12 cores"), "{text}");
        assert!(text.contains("64.9 GB available"), "{text}");
        assert!(text.contains("0 more fit"), "{text}");
        assert!(text.contains("0.0s ago"), "{text}");
    }

    #[test]
    fn ac1_hp_minimized_block_is_three_glance_lines() {
        let lines = opened(live()).minimized_lines(&AC6_AGES);

        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("107.3 of 96.0 max"), "{lines:?}");
        assert!(lines[0].contains("1.1x"), "{lines:?}");
        assert!(lines[1].contains("80% busy of 12 cores"), "{lines:?}");
        assert!(
            lines[2].contains("2 working · 2 idle · 1 stale · 1 dead · 1 unknown age"),
            "{lines:?}"
        );
        assert!(lines[2].contains("(7 rows)"), "{lines:?}");
    }

    #[test]
    fn ac2_hp_a_toggle_keeps_the_reading() {
        let mut panel = opened(live());
        panel.toggle();
        assert!(panel.is_expanded());
        panel.toggle();
        assert!(!panel.is_expanded());
        // Expanding never spawns a fold and never drops the cached reading.
        assert!(!panel.take_want(), "inside the TTL no refetch");
        assert!(panel
            .expanded_lines(&AC6_AGES)
            .join("\n")
            .contains("5 kings"));
    }

    #[test]
    fn ac5_edge_a_dark_load_arm_names_its_reason_rather_than_printing_a_number() {
        let court = parse(
            br#"{"lane_count": null, "refused_reason": "arms dark",
                 "census": {}, "arms": [{"name": "spawn load", "state": "dark",
                 "value": null, "reason": "load average unreadable"}]}"#,
        )
        .expect("parses");

        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(
            text.contains("load avg unknown - load average unreadable"),
            "{text}"
        );
        assert!(!text.contains("load avg 107.3"), "{text}");
    }

    #[test]
    fn ac6_hp_the_census_renders_a_split_not_a_total() {
        let text = opened(live()).expanded_lines(&AC6_AGES).join("\n");

        assert!(
            text.contains("2 working · 2 idle · 1 stale · 1 dead · 1 unknown age (7 rows)"),
            "{text}"
        );
        assert!(
            text.contains("1 dead: run `fno agents reap --apply`"),
            "{text}"
        );
        // The lanes workers count is never rendered: the split replaced it.
        assert!(!text.contains("45 workers"), "{text}");
    }

    #[test]
    fn ac7_edge_no_age_anywhere_is_never_a_live_bucket() {
        let ages = [None::<u64>, None, None];
        let text = opened(live()).expanded_lines(&ages).join("\n");

        assert!(text.contains("3 unknown age (3 rows)"), "{text}");
        assert!(!text.contains("0 working"), "{text}");
        assert!(!text.contains("0 idle"), "{text}");
        assert!(!text.contains("0 stale"), "{text}");
        assert!(!text.contains("0 dead"), "{text}");
        assert!(!text.contains("reap"), "{text}");
    }

    #[test]
    fn ac8_hp_top_consumers_name_the_process_and_the_worktree() {
        let text = opened(live()).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("fno-py 23 procs 41%"), "{text}");
        assert!(
            text.contains("22 of fno-py in .fno/worktrees/x-b1ee"),
            "{text}"
        );
        assert!(
            text.contains("18 of fno-agents-worker in .fno/worktrees/x-b1ee"),
            "{text}"
        );
    }

    #[test]
    fn ac9_edge_an_absent_top_block_degrades_only_the_top_line() {
        let mut court = live();
        court.census.top_consumers = None;
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(
            text.contains("unavailable - the fleet footprint reading is dark"),
            "{text}"
        );
        assert!(text.contains("load avg 107.3 now"), "{text}");
        assert!(text.contains("80% busy of 12 cores"), "{text}");
        assert!(text.contains("2 working"), "{text}");
    }

    #[test]
    fn a_saturated_box_points_at_the_top_block() {
        // The 100%-busy reading was TRUE and read as a bug: the saturated
        // line must name where the explanation lives.
        let mut court = live();
        let arm = court
            .arms
            .iter_mut()
            .find(|a| a.name == "whole-machine cpu")
            .expect("the cpu arm exists");
        arm.value["busy_fraction"] = serde_json::json!(1.0);
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("(all 12 saturated; see top)"), "{text}");
    }

    #[test]
    fn a_pending_fold_minimized_says_so_rather_than_showing_zeroes() {
        let panel = Panel::default();

        let lines = panel.minimized_lines(&[]);

        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("reading the machine"), "{lines:?}");
        assert!(
            !lines[0].contains('0'),
            "a pending fold must show no counts"
        );
        assert!(lines[1].is_empty() && lines[2].is_empty(), "{lines:?}");
    }

    #[test]
    fn a_failed_fold_expanded_is_a_named_degrade_never_a_blank_panel() {
        let mut panel = Panel::default();
        assert!(panel.take_want());
        panel.apply(None);

        let lines = panel.expanded_lines(&[]);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("fold failed"), "{lines:?}");
        assert!(lines[0].contains("10s"), "{lines:?}");
    }

    #[test]
    fn a_fold_failure_minimized_names_the_retry() {
        let mut panel = Panel::default();
        assert!(panel.take_want());
        panel.apply(None);

        let lines = panel.minimized_lines(&[]);

        assert!(lines[0].contains("fold failed - retrying"), "{lines:?}");
    }

    #[test]
    fn a_failed_fold_backs_off_to_the_next_ttl_boundary() {
        let mut panel = Panel::default();
        assert!(panel.take_want());
        panel.apply(None);

        // The failure armed a retry delay, so the always-visible block does
        // not turn an instantly-failing fold into a hot refetch loop.
        assert!(!panel.take_want());
        assert!(panel.refresh_deadline().is_some_and(|t| t > Instant::now()));
    }

    #[test]
    fn a_failed_refresh_over_a_stale_reading_never_leaves_a_past_due_deadline() {
        // The spin: a stale reading arms a refresh, the refresh fails, and
        // the retry backoff starts from NOW while the stale reading's due
        // time is already long past. The timer wake must wait for the
        // backoff, not re-fire every pass until it ends.
        let mut panel = opened(live());
        panel.fold_at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(120));
        assert!(panel.take_want());
        panel.apply(None);

        assert!(!panel.take_want(), "still inside the retry backoff");
        assert!(panel.refresh_deadline().is_some_and(|t| t > Instant::now()));
    }

    #[test]
    fn a_failed_refresh_over_a_good_reading_keeps_the_numbers_and_says_so() {
        let mut panel = opened(live());
        panel.fold_at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        assert!(panel.take_want());
        panel.apply(None);

        let text = panel.expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("last refresh failed"), "{text}");
        assert!(text.contains("5 kings"), "{text}");
        // The failed refresh did NOT restamp the age: the line still
        // describes the last real reading, which is already stale.
        assert!(
            text.contains("stale, refreshing) · last refresh failed"),
            "{text}"
        );
    }

    #[test]
    fn a_successful_refresh_clears_the_failure_note() {
        let mut panel = opened(live());
        panel.fold_at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        assert!(panel.take_want());
        panel.apply(None);
        // The failure backed off one TTL; simulate it elapsing.
        panel.retry_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(panel.take_want());
        panel.apply(Some(live()));

        assert!(!panel
            .expanded_lines(&AC6_AGES)
            .join("\n")
            .contains("last refresh failed"));
    }

    #[test]
    fn a_reading_inside_the_ttl_spawns_no_refold() {
        let mut panel = opened(live());

        assert!(!panel.take_want(), "no refetch inside the TTL");
        assert!(panel.refresh_deadline().is_some_and(|t| t > Instant::now()));
    }

    #[test]
    fn a_reading_past_the_ttl_refetches_and_keeps_showing_the_stale_reading() {
        let mut panel = opened(live());
        panel.fold_at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));

        assert!(panel.take_want(), "a stale reading refetches");
        let text = panel.expanded_lines(&AC6_AGES).join("\n");
        assert!(text.contains("stale, refreshing"), "{text}");
        assert!(text.contains("5 kings"), "{text}");
        // While the refetch runs, no timer wake is armed: a past-due
        // deadline would re-fire every pass for nothing.
        assert!(panel.refresh_deadline().is_none());
    }

    #[test]
    fn an_empty_roster_says_so_instead_of_a_zero() {
        let text = opened(live()).expanded_lines(&[]).join("\n");

        assert!(text.contains("no roster rows held"), "{text}");
    }

    #[test]
    fn the_attribution_gap_gets_its_own_line_and_never_a_count() {
        // x-e040 made the gap honest. A panel that folded it into a count
        // would re-open the hole: the gap is a process-to-row failure and
        // cannot change how many rows exist.
        let mut court = live();
        court.census.attribution_gap =
            Some("11 pidless row(s) with no identity route (codex)".to_string());
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("11 pidless row(s)"), "{text}");
        assert!(text.contains("undercount, not headroom"), "{text}");
        assert!(text.contains("5 kings"), "{text}");
    }

    #[test]
    fn an_unreadable_census_renders_unknown_never_a_fabricated_zero() {
        let mut court = live();
        court.census = Census {
            tests: Some(3),
            ..Census::default()
        };
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("unknown kings · 3 test processes"), "{text}");
    }

    #[test]
    fn a_king_conflict_is_warned_because_a_bare_count_hides_it() {
        let mut court = live();
        court.census.king_conflicts = Some(2);

        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(
            text.contains("2 scope(s) held by more than one crown"),
            "{text}"
        );
    }

    #[test]
    fn a_refusal_carries_the_advisors_words_and_no_lane_number() {
        let mut court = live();
        court.lane_count = None;
        court.refused_reason =
            "the machine arms cannot answer the lane question: memory dark (macmon not on PATH)"
                .to_string();
        let text = opened(court).expanded_lines(&AC6_AGES).join("\n");

        assert!(text.contains("lanes    REFUSED"), "{text}");
        assert!(text.contains("memory dark (macmon not on PATH)"), "{text}");
        assert!(!text.contains("more fit"), "{text}");
    }

    #[test]
    fn only_one_fold_is_ever_in_flight() {
        let mut panel = Panel::default();

        assert!(panel.take_want());
        assert!(
            !panel.take_want(),
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

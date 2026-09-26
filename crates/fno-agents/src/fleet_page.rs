//! The private HTML projection of the fleet load report and its daemon arm.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;

use crate::fleet_load::{self, FleetReport};
use crate::paths::AgentsHome;
use crate::transcript_activity::{Roots, TICK_READ_BUDGET};

pub const FLEET_PAGE_INTERVAL_S: u64 = 1800;

/// The daemon-owned page arm: config cwd plus a cadence stamp and a single
/// blocking render in flight.
pub struct Arm {
    config_cwd: PathBuf,
    last_tick: Mutex<Option<std::time::Instant>>,
    in_flight: Arc<AtomicBool>,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Self {
            config_cwd,
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Render report JSON into the page template. Escaping `<` in the data blob
/// keeps operator-controlled slowdown text inside the JSON script element.
pub(crate) fn render(report: &FleetReport, generated: &str, reload_s: i64) -> String {
    let data = serde_json::to_string(report)
        .unwrap_or_else(|_| "{}".to_string())
        .replace('<', "\\u003c");
    let template = include_str!("fleet_page.html");
    let mut out = template.replacen("/*DATA*/null", &data, 1).replace(
        "__GENERATED__",
        &crate::claude_ask::html_escape_quote(generated),
    );
    out.push_str(&format!(
        "<script data-fno-reload=\"{reload_s}\">{}</script></body></html>",
        crate::king_ledger::PAGE_RELOAD_JS
    ));
    out
}

pub(crate) fn write_page(path: &Path, body: &str) -> Result<(), String> {
    crate::king_ledger::write_atomic(&path.to_path_buf(), body)
}

fn run_page(config_cwd: &Path) -> Result<String, String> {
    let state_dir = crate::agents_config::state_dir(config_cwd)
        .ok_or_else(|| "no state dir resolves from the daemon cwd".to_string())?;
    let report = fleet_load::analyze(&fleet_load::Inputs {
        home: AgentsHome::from_env(),
        state_dir: state_dir.clone(),
        roots: Roots {
            claude_projects: crate::claude_drive::claude_projects_dir(),
            codex_sessions: None,
            bus_log: crate::intel::bus_log_path(&state_dir),
        },
        now: Utc::now(),
        window_days: fleet_load::DEFAULT_WINDOW_DAYS,
        budget: Some(TICK_READ_BUDGET),
    });
    let reload_s = crate::king_ledger::reload_secs(crate::agents_config::config_lookup(
        config_cwd,
        &["backlog", "page_reload_s"],
    ));
    let body = render(&report, &Utc::now().to_rfc3339(), reload_s);
    write_page(&state_dir.join("fleet.html"), &body)?;
    Ok(format!(
        "fleet.html rendered: {} readings, {} transcripts read, {} pending",
        report.coverage.readings.rows,
        report.coverage.fold.files_read,
        report.coverage.fold.pending_files
    ))
}

fn emit_one(
    home: &AgentsHome,
    run: impl FnOnce() -> Result<String, String>,
) -> crate::merge_close::CloseOutcome {
    let outcome = match run() {
        Ok(detail) => crate::merge_close::CloseOutcome {
            acted: 1,
            skip_reason: None,
            detail,
        },
        Err(error) => crate::merge_close::CloseOutcome {
            acted: 0,
            skip_reason: Some("error".to_string()),
            detail: error.chars().take(200).collect(),
        },
    };
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "fleet_page",
        crate::tick_ledger::SCHED_DAEMON,
        outcome.acted,
        outcome.skip_reason.as_deref(),
        Some(&outcome.detail),
        FLEET_PAGE_INTERVAL_S,
    );
    outcome
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let config_cwd = arm.config_cwd.clone();
    maybe_tick_with(arm, home, move || run_page(&config_cwd));
}

fn maybe_tick_with(
    arm: &Arm,
    home: AgentsHome,
    run: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    let interval = std::time::Duration::from_secs(FLEET_PAGE_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|stamp| stamp.elapsed() < interval)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        emit_one(&home, run);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_load::{FleetReport, SlowdownRow};

    fn home() -> AgentsHome {
        let dir = std::env::temp_dir().join(format!(
            "fno-fleet-page-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        AgentsHome::at(dir.join("agents"))
    }

    #[test]
    fn render_escapes_slowdown_script_and_keeps_empty_states() {
        let report = FleetReport {
            slowdowns: vec![SlowdownRow {
                ts: "2026-09-19T00:00:00Z".into(),
                session: "s".into(),
                harness: "codex".into(),
                text: "</script><script>alert(1)</script>".into(),
                reading: None,
            }],
            ..FleetReport::default()
        };
        let page = render(&report, "2026-09-19T00:00:00Z", 60);
        let data_start = page.find("const DATA=").unwrap();
        let data_end = page[data_start..].find(";\nconst GENERATED").unwrap() + data_start;
        assert!(!page[data_start..data_end].contains("</script>"));
        assert!(page.contains(
            "No structured machine sample yet. Memory, compressor and swap are not recorded."
        ));
        assert!(page.contains("data-fno-reload=\"60\""));
    }

    #[test]
    fn emit_one_records_success_and_failure() {
        let success_home = home();
        let success = emit_one(&success_home, || {
            Ok("fleet.html rendered: 1 readings".into())
        });
        assert_eq!(success.acted, 1);
        let success_log = crate::events::committed_journal_text(&success_home.events_jsonl());
        assert!(success_log.contains("\"arm\":\"fleet_page\""));
        assert!(success_log.contains("\"interval_s\":1800"));

        let failure_home = home();
        let failure = emit_one(&failure_home, || Err("cannot read state".into()));
        assert_eq!(failure.acted, 0);
        assert_eq!(failure.skip_reason.as_deref(), Some("error"));
        let failure_log = crate::events::committed_journal_text(&failure_home.events_jsonl());
        assert!(failure_log.contains("\"acted\":0"));
        assert!(failure_log.contains("\"skip_reason\":\"error\""));
    }

    #[tokio::test]
    async fn young_cadence_stamp_writes_no_row() {
        let arm = Arm::new(PathBuf::from("/tmp/fno-fleet-page-test"));
        let home = home();
        *arm.last_tick.lock().unwrap() = Some(std::time::Instant::now());
        maybe_tick_with(&arm, home.clone(), || panic!("arm must be gated"));
        tokio::task::yield_now().await;
        assert!(!home.events_jsonl().exists());
    }
}

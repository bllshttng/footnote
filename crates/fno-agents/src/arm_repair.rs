//! Why a red arm is red, the one verb that repairs it, and who runs that verb.
//!
//! One cause table for every reader (`fno agents status`, the arm_watch
//! notice, the king check-in). `annotate` classifies each red row and ends its
//! line with `repair: <verb>` and `heal=auto|operator`. `heal` runs the
//! `heal=auto` repairs from the arm_watch tick, before anything pages.

use std::path::{Path, PathBuf};

use crate::stuck_work::Finding;
use crate::tick_ledger::{needs_attention, render_row, ArmStatus, ProducerEvidence};
use crate::tick_ledger::{SCHED_DAEMON, SCHED_LAUNCHD};

const AUTO: &str = "auto";
const OPERATOR: &str = "operator";
const REFRESH: &str = "fno do pr watch refresh";
const RESTART: &str = "fno agents restart";
const WATCH_STATUS: &str = "fno do pr watch status";
const SCHEDULER_DOWN_HINT: &str =
    "every arm on this scheduler is silent; the job is not running, the arm is fine";

/// The cause table: hint, repair verb, heal owner. An owner of `None` means
/// the cause is not a fault (the arm is off, or the daemon is young).
fn entry(
    cause: &str,
    scheduler: Option<&str>,
) -> (&'static str, Option<&'static str>, Option<&'static str>) {
    match cause {
        "tick_overdue" => (
            "no tick stamp inside 2x interval",
            Some(REFRESH),
            Some(AUTO),
        ),
        "launchd_foreign_plist" => (
            "sh.fno.pr-watcher is registered from a foreign plist",
            Some(REFRESH),
            Some(AUTO),
        ),
        "scheduler_down" if scheduler == Some(SCHED_LAUNCHD) => {
            (SCHEDULER_DOWN_HINT, Some(REFRESH), Some(AUTO))
        }
        "scheduler_down" if scheduler == Some(SCHED_DAEMON) => {
            (SCHEDULER_DOWN_HINT, Some(RESTART), Some(OPERATOR))
        }
        "scheduler_down" => (SCHEDULER_DOWN_HINT, None, Some(OPERATOR)),
        "stale_daemon" => (
            "daemon predates the installed build; run fno agents restart",
            Some(RESTART),
            Some(OPERATOR),
        ),
        "daemon_down" => ("daemon not running", Some(RESTART), Some(OPERATOR)),
        "tick_timeout" => (
            "the pr-watch tick cut this arm's phase; run fno do pr watch status",
            Some(WATCH_STATUS),
            Some(OPERATOR),
        ),
        "timeout" => (
            "the arm's run hit its time limit; the step named in the detail is where the clock stopped, not a measured cause",
            Some(WATCH_STATUS),
            Some(OPERATOR),
        ),
        "budget_spent" => (
            "the pass ran out of its slice before it covered every unit it enumerated; the detail names how many of N it reached",
            Some(WATCH_STATUS),
            Some(OPERATOR),
        ),
        "dead_holder" => (
            "the flight holder's pid is gone and nothing reacquires the scope",
            None,
            Some(AUTO),
        ),
        "stale_build" => (
            "the installed build is not from the main checkout",
            Some("fno doctor update"),
            Some(AUTO),
        ),
        "parent_gone" => (
            "the verb's parent process exited under it",
            Some("fno backlog reconcile --json"),
            Some(OPERATOR),
        ),
        "select_unmeasured" => (
            "the next-node read did not answer inside auto_continue.select_timeout_s; the heal lane retries it",
            None,
            Some(AUTO),
        ),
        "configured_off" => (
            "the arm is off in config; its age is the switch, not a dead scheduler",
            None,
            None,
        ),
        "daemon_young" => ("daemon up, first window not elapsed", None, None),
        "unexplained" => (
            "scheduler looks healthy; the arm itself did not tick",
            None,
            Some(OPERATOR),
        ),
        "fleet_stop" => (
            "the fleet incident breaker is armed; held on purpose, wait for it to clear",
            Some("fno agents incident status"),
            None,
        ),
        "loops_paused" => (
            "loops are paused by hand; wait for resume-all",
            Some("fno do loops status"),
            None,
        ),
        "fleet_stop_unavailable" => (
            "the fleet incident record is unreadable; every loop pauses until it reads",
            Some("fno agents incident status"),
            Some(OPERATOR),
        ),
        "unarmed" => (
            "this loop is off by config; the switch, not a scheduler, owns its silence",
            None,
            None,
        ),
        "starved" => (
            "armed and running, producing nothing; its input has been empty",
            None,
            None,
        ),
        "cwd_not_checkout" => (
            "a node's recorded cwd is not a git checkout, so gh cannot find its repository",
            None,
            Some(OPERATOR),
        ),
        _ => (
            "no class matches; read the skip reason and detail",
            None,
            Some(OPERATOR),
        ),
    }
}

/// The hint text for a cause token.
pub fn hint(cause: &str) -> &'static str {
    entry(cause, None).0
}

/// The front-door release verb for one claim. The root prefix is printed
/// whenever the claim is not under `$HOME`: the verb is pasted into another
/// shell, which may not carry the reader's `FNO_CLAIMS_ROOT`. No root means
/// the global root.
pub fn release_verb(key: &str, holder: &str, root: Option<&Path>) -> String {
    let prefix = match root {
        Some(r) if std::env::var_os("HOME").as_deref() != Some(r.as_os_str()) => {
            format!("FNO_CLAIMS_ROOT={} ", r.display())
        }
        _ => String::new(),
    };
    format!("{prefix}fno agents claim release {key} --holder {holder}")
}

/// What `annotate` knows beyond the rows.
pub struct RepairFacts {
    /// The install pin says the installed build is not from the main checkout.
    pub install_off_main: bool,
    /// (claim key, holder, claims root) for each dead holder the stuck-work
    /// read found. A held row whose receipt has no key looks its key up here.
    pub dead_holds: Vec<(String, String, Option<PathBuf>)>,
    /// True when the pid is gone on this host.
    pub pid_dead: fn(i32) -> bool,
}

impl RepairFacts {
    pub fn new(install_off_main: bool, findings: &[Finding]) -> Self {
        Self {
            install_off_main,
            dead_holds: findings
                .iter()
                .filter_map(|f| Some((f.claim_key.clone()?, f.holder.clone()?, f.root.clone())))
                .collect(),
            pid_dead: |pid| {
                matches!(
                    crate::claims::probe_pid(pid),
                    crate::claims::PidProbe::Absent
                )
            },
        }
    }

    /// The facts from this machine: the install pin under `~/.fno`.
    pub fn live(findings: &[Finding]) -> Self {
        let pin = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".fno").join("source-pin.json"))
            .and_then(|p| std::fs::read_to_string(p).ok());
        Self::new(install_off_main(pin.as_deref()), findings)
    }
}

/// Whether the install pin proves the build came from off main: a linked
/// worktree, or a source the pin gate called divergent. A missing or
/// unreadable pin is no evidence, and a packaged install is not a worktree.
pub fn install_off_main(pin_text: Option<&str>) -> bool {
    let Some(pin) = pin_text.and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok()) else {
        return false;
    };
    let field = |name: &str| pin.get(name).and_then(serde_json::Value::as_str);
    field("worktree_kind") == Some("linked_worktree") || field("eligibility") == Some("divergent")
}

/// `explain_with_trace`, then `annotate` with this machine's facts: the one
/// call `fno agents status` makes, so the client prints annotated rows.
pub fn explain(
    rows: &mut [ArmStatus],
    daemon: &crate::tick_ledger::DaemonFacts,
    trace: &crate::tick_ledger::TickTrace,
) {
    crate::tick_ledger::explain_with_trace(rows, daemon, trace);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let findings = crate::stuck_work::collect(&cwd).unwrap_or_default();
    annotate(rows, &RepairFacts::live(&findings));
}

/// Classify every red row: a cause, a repair verb, and an owner, all printed
/// on the row's line. Run once per read, after `explain`.
pub fn annotate(rows: &mut [ArmStatus], facts: &RepairFacts) {
    for row in rows.iter_mut() {
        classify(row, facts);
    }
    // The upstream edge wins over the row's own verdict: an arm that waits on
    // a red arm is not itself broken. An upstream with no owner is off in
    // config, young, or unobserved - not down.
    let red: Vec<(String, Option<String>, Option<String>)> = rows
        .iter()
        .filter(|r| needs_attention(r) && r.heal.is_some())
        .map(|r| (r.arm.clone(), r.repair.clone(), r.heal.clone()))
        .collect();
    for row in rows.iter_mut() {
        if !needs_attention(row) || row.producer_evidence == ProducerEvidence::Unobserved {
            continue;
        }
        let Some(up) = crate::tick_ledger::upstream_of(&row.arm) else {
            continue;
        };
        let Some((_, repair, heal)) = red.iter().find(|(arm, ..)| arm == up) else {
            continue;
        };
        row.cause = Some("upstream_down".to_string());
        row.upstream = Some(up.to_string());
        row.repair = repair.clone();
        row.heal = heal.clone();
        row.line = format!(
            "{} cause=upstream_down ({up} is down; this arm waits on it){}",
            render_row(row),
            suffix(row)
        );
    }
}

fn classify(row: &mut ArmStatus, facts: &RepairFacts) {
    if row.producer_evidence == ProducerEvidence::Unobserved {
        // No receipt, no measured cause.
        return;
    }
    // A paused row is held on purpose, not attention-worthy: it still
    // carries a read verb, never an action, and never a heal owner.
    if let Some(cause) = row
        .cause
        .as_deref()
        .filter(|c| matches!(*c, "fleet_stop" | "loops_paused"))
        .map(str::to_string)
    {
        let (hint, verb, _) = entry(&cause, row.scheduler.as_deref());
        row.repair = verb.map(str::to_string);
        if row.line.is_empty() {
            row.line = format!("{} cause={cause} ({hint})", render_row(row));
        }
        row.line.push_str(&suffix(row));
        return;
    }
    // The two vocabulary words: neither is a fault, so neither ends in
    // `heal=auto`. An unarmed row names the switch that turns the loop on; a
    // starved row names the verb that reads the loop's own detail.
    if crate::tick_ledger::row_is_unarmed(row) {
        let key = row.arm_key.clone().unwrap_or_default();
        row.cause = Some("unarmed".to_string());
        row.repair = Some(format!("fno config set {key} true"));
        if row.line.is_empty() {
            let (hint, _, _) = entry("unarmed", row.scheduler.as_deref());
            row.line = format!("{} cause=unarmed ({hint})", render_row(row));
        }
        row.line.push_str(&suffix(row));
        return;
    }
    if row.starved {
        row.cause = Some("starved".to_string());
        row.repair = Some(
            row.reader
                .clone()
                .unwrap_or_else(|| "fno agents loops table".to_string()),
        );
        if row.line.is_empty() {
            let (hint, _, _) = entry("starved", row.scheduler.as_deref());
            row.line = format!("{} cause=starved ({hint})", render_row(row));
        }
        row.line.push_str(&suffix(row));
        return;
    }
    let held_dead = held_dead_holder(row, facts);
    if !needs_attention(row) && held_dead.is_none() {
        return;
    }
    if matches!(
        row.cause.as_deref(),
        Some("configured_off") | Some("daemon_young")
    ) {
        return;
    }
    let sched = row.scheduler.clone();
    let on_tier = matches!(sched.as_deref(), Some(SCHED_LAUNCHD) | Some(SCHED_DAEMON));
    let skip = row.skip_reason.clone().unwrap_or_default();
    let detail = row.detail.clone().unwrap_or_default();
    let (cause, repair) = if let Some((verb, held_for_s)) = held_dead {
        row.failing = true;
        row.failing_for_s = Some(held_for_s);
        ("dead_holder".to_string(), verb)
    } else if facts.install_off_main && on_tier {
        ("stale_build".to_string(), None)
    } else if let Some(cause) = row.cause.clone() {
        // `explain` already named the cause and its hint: add only the repair.
        let (hint, verb, heal) = entry(&cause, sched.as_deref());
        row.repair = if cause == "select_unmeasured" {
            Some(select_unmeasured_repair(&detail))
        } else {
            verb.map(str::to_string)
        };
        row.heal = heal.map(str::to_string);
        if row.line.is_empty() {
            row.line = format!("{} cause={cause} ({hint})", render_row(row));
        }
        row.line.push_str(&suffix(row));
        return;
    } else if row.failing && detail.contains("parent-gone") {
        ("parent_gone".to_string(), None)
    } else if skip == "select-unmeasured" {
        (
            "select_unmeasured".to_string(),
            Some(select_unmeasured_repair(&detail)),
        )
    } else if skip == "budget_spent" {
        ("budget_spent".to_string(), None)
    } else if skip == "timeout" {
        ("timeout".to_string(), None)
    } else if detail.contains("not a git repository") {
        (
            "cwd_not_checkout".to_string(),
            Some(cwd_not_checkout_repair(&detail)),
        )
    } else {
        ("unclassified".to_string(), None)
    };
    let (hint, verb, heal) = entry(&cause, sched.as_deref());
    row.cause = Some(cause.clone());
    row.repair = repair.or_else(|| verb.map(str::to_string));
    row.heal = heal.map(str::to_string);
    row.line = format!("{} cause={cause} ({hint}){}", render_row(row), suffix(row));
}

/// The project a select-read unmeasured detail names; `None` when it names
/// none (`-`).
fn project_from_detail(detail: &str) -> Option<&str> {
    detail
        .split_once(crate::select_read::PROJECT_TOKEN)
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .filter(|project| *project != "-")
}

fn select_unmeasured_repair(detail: &str) -> String {
    match project_from_detail(detail) {
        Some(project) => format!("fno backlog advance --project {project} --source ac --json"),
        None => "fno backlog advance --source ac --json".to_string(),
    }
}

/// The node id a merge_close detail names after `first: `; `None` when the
/// detail names none.
fn node_from_detail(detail: &str) -> Option<&str> {
    detail
        .split_once("first: ")?
        .1
        .split_whitespace()
        .next()
        .filter(|id| *id != "?")
}

/// The repair for `cwd_not_checkout`. The `<p>`/`<checkout>` placeholders stay
/// literal: the right checkout is a human judgment, not a resolver guess.
fn cwd_not_checkout_repair(detail: &str) -> String {
    let id = node_from_detail(detail).unwrap_or("<id>");
    format!("fno backlog update {id} --project <p> --cwd <checkout>")
}

fn suffix(row: &ArmStatus) -> String {
    let mut out = String::new();
    if let Some(verb) = &row.repair {
        out.push_str(&format!(" repair: {verb}"));
    }
    if let Some(owner) = &row.heal {
        out.push_str(&format!(" heal={owner}"));
    }
    out
}

/// A `skip=held` row whose holder pid is gone: the release verb (when the key
/// is known) and how long the scope has been held. The detail shapes are the
/// merge_close receipt: `flight <key> held by <holder> for <n>s`, or the
/// keyless `flight held by <holder> for <n>s`.
fn held_dead_holder(row: &ArmStatus, facts: &RepairFacts) -> Option<(Option<String>, u64)> {
    if row.skip_reason.as_deref() != Some("held") {
        return None;
    }
    let rest = row.detail.as_deref()?.strip_prefix("flight ")?;
    let (key, after) = rest.split_once("held by ")?;
    let (holder, tail) = after.split_once(" for ")?;
    let held_for_s: u64 = tail.trim().trim_end_matches('s').parse().ok()?;
    if held_for_s <= crate::stuck_work::DEAD_HOLDER_GRACE_S as u64 {
        // The gate reclaims a fresh corpse on its next acquire.
        return None;
    }
    let pid: i32 = holder
        .strip_prefix("single-flight:")?
        .split(':')
        .next()?
        .parse()
        .ok()?;
    if !(facts.pid_dead)(pid) {
        return None;
    }
    let key = key.trim();
    let known = facts
        .dead_holds
        .iter()
        .find(|(k, h, _)| h == holder && (key.is_empty() || k == key));
    let verb = match (key.is_empty(), known) {
        (_, Some((k, h, Some(root)))) => Some(release_verb(k, h, Some(root))),
        (false, _) => Some(release_verb(key, holder, None)),
        // A keyless receipt: the stuck-work block prints the exact verb.
        (true, _) => Some("fno agents status".to_string()),
    };
    Some((verb, held_for_s))
}

/// The heal lane. Runs the `heal=auto` repairs and returns the token the
/// arm_watch tick detail carries: `heal=off`, `heal=0`, or the actions taken
/// (`heal=dead_holder:2,refresh:ok`). `run` executes `refresh` or `install`
/// and says whether it started cleanly.
pub fn heal(
    rows: &[ArmStatus],
    findings: &[Finding],
    enabled: bool,
    store: &Path,
    now_unix: u64,
    threshold_s: u64,
    run: &mut dyn FnMut(&str) -> bool,
) -> String {
    if !enabled {
        return "heal=off".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let (mut released, mut skipped) = (0, 0);
    for f in findings.iter().filter(|f| f.kind == "dead_holder") {
        let (Some(key), Some(holder), Some(root)) = (&f.claim_key, &f.holder, &f.root) else {
            skipped += 1;
            continue;
        };
        if release_dead(key, holder, root) {
            released += 1;
        } else {
            skipped += 1;
        }
    }
    if released > 0 {
        parts.push(format!("dead_holder:{released}"));
    }
    if skipped > 0 {
        parts.push(format!("skipped:{skipped}"));
    }
    // The breaker covers the whole fleet: no self-heal action runs under it.
    // The dead-holder lane above stays, because releasing a dead pid's claim
    // frees a scope without reinstalling anything.
    if rows.iter().any(|r| {
        matches!(
            r.cause.as_deref(),
            Some("fleet_stop") | Some("loops_paused") | Some("fleet_stop_unavailable")
        )
    }) {
        parts.clear();
        parts.push("paused".to_string());
        return format!("heal={}", parts.join(","));
    }
    // Rows carrying the two vocabulary words are never repairs: an off or
    // idle loop is a configuration, and the token names the stand-down so a
    // quiet heal tick is a decision, not a blind spot.
    let stood_down = rows
        .iter()
        .filter(|r| r.starved || crate::tick_ledger::row_is_unarmed(r))
        .count();
    if stood_down > 0 {
        parts.push(format!("stand_down:{stood_down}"));
    }
    let mut refresh_set: Vec<String> = rows
        .iter()
        .filter(|r| {
            r.scheduler.as_deref() == Some(SCHED_LAUNCHD)
                && matches!(
                    r.cause.as_deref(),
                    Some("tick_overdue") | Some("scheduler_down") | Some("launchd_foreign_plist")
                )
                && r.age_s.is_some_and(|s| s >= threshold_s)
        })
        .map(|r| format!("{}@{}", r.arm, r.last_ts.as_deref().unwrap_or("never")))
        .collect();
    refresh_set.sort();
    // ponytail: one refresh per episode (the stale set is the token), add a retry clock if one refresh is not enough
    if !refresh_set.is_empty()
        && crate::operator_notice::mark_once(store, "self_heal:refresh", &refresh_set.join(","))
    {
        let ok = run("refresh");
        parts.push(format!("refresh:{}", if ok { "ok" } else { "failed" }));
    }
    // ponytail: one install per 6h bucket, the pin read on the next tick is the only result check
    if rows
        .iter()
        .any(|r| r.cause.as_deref() == Some("stale_build"))
        && crate::operator_notice::mark_once(
            store,
            "self_heal:install",
            &(now_unix / 21_600).to_string(),
        )
    {
        let ok = run("install");
        parts.push(format!("install:{}", if ok { "spawned" } else { "failed" }));
    }
    let mut advance_rows: Vec<&ArmStatus> = rows
        .iter()
        .filter(|r| r.cause.as_deref() == Some("select_unmeasured"))
        .collect();
    advance_rows.sort_by_key(|r| r.last_ts.as_deref().unwrap_or("never"));
    for row in advance_rows {
        let token = row.last_ts.as_deref().unwrap_or("never");
        if !crate::operator_notice::mark_once(store, "self_heal:advance", token) {
            continue;
        }
        let project = row
            .detail
            .as_deref()
            .and_then(project_from_detail)
            .unwrap_or("-");
        let action = format!("advance:{project}");
        let ok = run(&action);
        parts.push(format!(
            "{action}:{}",
            if ok { "spawned" } else { "failed" }
        ));
    }
    if parts.is_empty() {
        "heal=0".to_string()
    } else {
        format!("heal={}", parts.join(","))
    }
}

/// Release one dead hold, holder-bound: re-read the claim and re-probe its pid
/// right before the release, so a live replacement is never dropped.
fn release_dead(key: &str, holder: &str, root: &Path) -> bool {
    let (_, Some(rec)) = crate::claims::status(key, Some(root)) else {
        return false;
    };
    if rec.holder != holder {
        return false;
    }
    let dead = rec.pid.is_some_and(|pid| {
        matches!(
            crate::claims::probe_pid(pid),
            crate::claims::PidProbe::Absent
        )
    });
    dead && crate::claims::release(key, holder, Some(root), None).is_ok()
}

/// The production runner for `heal`: the launchd refresh under a 120 s bound,
/// and the install spawned detached from the canonical checkout.
pub fn run_repair(action: &str, cwd: &Path) -> bool {
    match action {
        "refresh" => {
            let mut cmd = vec![crate::scrape::fno_py().to_string_lossy().into_owned()];
            cmd.extend(["do", "pr", "watch", "refresh"].map(str::to_string));
            crate::king_board::budget::run_with_timeout(
                &cmd,
                cwd,
                std::time::Duration::from_secs(120),
            )
            .is_ok()
        }
        "install" => {
            let Some(root) = crate::paths::canonical_repo_root(cwd) else {
                return false;
            };
            let child = std::process::Command::new(crate::scrape::fno_py())
                .args(["doctor", "update"])
                .current_dir(root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                return false;
            };
            // Reap it off the tick, so the daemon keeps no zombie.
            std::thread::spawn(move || child.wait());
            true
        }
        action if action.starts_with("advance:") => {
            let Some(root) = crate::paths::canonical_repo_root(cwd) else {
                return false;
            };
            let project = action.strip_prefix("advance:").unwrap_or("");
            let mut child = std::process::Command::new(crate::scrape::fno_py());
            child.args(["backlog", "advance"]);
            if !project.is_empty() && project != "-" {
                child.args(["--project", project]);
            }
            let child = child
                .args(["--source", "ac", "--json"])
                .current_dir(root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                return false;
            };
            std::thread::spawn(move || child.wait());
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{encode_key, hostname, machine_id, now_ms, SCHEMA_VERSION};
    use crate::tick_ledger::SCHED_LAUNCHD;

    fn row(arm: &str, sched: &str) -> ArmStatus {
        ArmStatus {
            arm: arm.to_string(),
            scheduler: Some(sched.to_string()),
            last_ts: Some("2026-09-04T12:00:00Z".to_string()),
            age_s: Some(30),
            acted: Some(0),
            skip_reason: None,
            detail: None,
            interval_s: 600,
            producer_evidence: ProducerEvidence::Observed,
            stale: false,
            failing: false,
            failing_for_s: None,
            cause: None,
            line: String::new(),
            repair: None,
            heal: None,
            upstream: None,
            arm_key: None,
            arm_value: None,
            reader: None,
            starved: false,
        }
    }

    fn facts(dead: bool) -> RepairFacts {
        RepairFacts {
            install_off_main: false,
            dead_holds: Vec::new(),
            pid_dead: if dead { |_| true } else { |_| false },
        }
    }

    fn dead_pid() -> i32 {
        let mut candidate = 999_999i32;
        while unsafe { libc::kill(candidate, 0) } == 0 {
            candidate += 1;
        }
        candidate
    }

    #[test]
    fn unarmed_rows_carry_the_config_verb_and_never_heal_auto() {
        let mut r = row("heal", SCHED_LAUNCHD);
        r.arm_key = Some("auto_heal.enabled".to_string());
        r.arm_value = Some("false".to_string());
        classify(&mut r, &facts(false));
        assert_eq!(r.cause.as_deref(), Some("unarmed"));
        assert!(
            r.line
                .ends_with("repair: fno config set auto_heal.enabled true"),
            "{}",
            r.line
        );
        assert!(!r.line.contains("heal=auto"), "{}", r.line);
        assert!(r.heal.is_none(), "{}", r.line);
    }

    #[test]
    fn starved_rows_carry_their_reader_verb() {
        let mut r = row("heal", "daemon");
        r.starved = true;
        r.reader = Some("fno do pr watch status".to_string());
        classify(&mut r, &facts(false));
        assert_eq!(r.cause.as_deref(), Some("starved"));
        assert!(
            r.line.contains("repair: fno do pr watch status"),
            "{}",
            r.line
        );
        assert!(!r.line.contains("heal=auto"), "{}", r.line);
    }

    #[test]
    fn the_heal_lane_names_the_stand_down_in_its_token() {
        let mut r = row("heal", SCHED_LAUNCHD);
        r.arm_key = Some("auto_heal.enabled".to_string());
        r.arm_value = Some("false".to_string());
        r.starved = false;
        classify(&mut r, &facts(false));
        let out = heal(
            std::slice::from_ref(&r),
            &[],
            true,
            Path::new("/tmp/does-not-matter"),
            0,
            1800,
            &mut |_| false,
        );
        assert!(
            out.contains("stand_down:1"),
            "token must name the stand-down: {out}"
        );
    }

    fn write_claim(root: &Path, key: &str, holder: &str, pid: i32) {
        let rec = crate::claims::ClaimRecord {
            schema_version: SCHEMA_VERSION,
            key: key.into(),
            holder: holder.into(),
            acquired_at: now_ms() - 600_000,
            pid: Some(pid),
            host: hostname(),
            pid_unavailable: false,
            expires_at: None,
            reason: None,
            harness: None,
            session_id: None,
            pid_provenance: None,
            machine_id: Some(machine_id()),
            metadata: Default::default(),
        };
        let dir = root.join(".fno/claims");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.lock", encode_key(key))),
            serde_json::to_string(&rec).unwrap(),
        )
        .unwrap();
    }

    fn holder_finding(root: &Path, key: &str, holder: &str) -> Finding {
        Finding {
            kind: "dead_holder",
            key: format!("holder:{key}@1"),
            line: format!("dead holder {key} holder {holder}"),
            root: Some(root.to_path_buf()),
            holder: Some(holder.to_string()),
            claim_key: Some(key.to_string()),
        }
    }

    // AC1: a stale auto_continue waiting on a failing pr_watch_merge reads UPSTREAM.
    #[test]
    fn a_waiting_arm_names_its_red_upstream() {
        let mut pm = row("pr_watch_merge", SCHED_LAUNCHD);
        pm.failing = true;
        pm.skip_reason = Some("timeout".into());
        pm.failing_for_s = Some(900);
        let mut ac = row("auto_continue", "session");
        ac.stale = true;
        ac.cause = Some("unexplained".into());
        let mut rows = vec![pm, ac];
        annotate(&mut rows, &facts(false));
        let ac = &rows[1];
        let words: Vec<&str> = ac.line.split_whitespace().take(2).collect();
        assert_eq!(words, ["auto_continue", "UPSTREAM"], "{}", ac.line);
        assert!(ac.line.contains("cause=upstream_down"), "{}", ac.line);
        assert!(ac.line.contains("pr_watch_merge"), "{}", ac.line);
        assert_eq!(ac.upstream.as_deref(), Some("pr_watch_merge"));
        assert_eq!(ac.repair, rows[0].repair);
        assert!(
            ac.line.contains("repair: fno do pr watch status"),
            "{}",
            ac.line
        );
    }

    // An upstream that is off in config is not down: the waiting arm keeps
    // its own cause and does not start paging.
    #[test]
    fn a_configured_off_upstream_is_not_down() {
        let mut pm = row("pr_watch_merge", SCHED_LAUNCHD);
        pm.stale = true;
        pm.skip_reason = Some("disabled".into());
        pm.cause = Some("configured_off".into());
        let mut ac = row("auto_continue", "session");
        ac.stale = true;
        ac.cause = Some("unexplained".into());
        let mut rows = vec![pm, ac];
        annotate(&mut rows, &facts(false));
        assert_eq!(rows[1].cause.as_deref(), Some("unexplained"));
        assert!(rows[1].upstream.is_none());
    }

    // AC2: a held row whose holder pid is dead reads FAIL dead_holder with a release verb.
    #[test]
    fn a_held_row_with_a_dead_holder_fails_with_a_release_verb() {
        let mut mc = row("merge_close", SCHED_DAEMON);
        mc.skip_reason = Some("held".into());
        mc.detail = Some("flight flight:k held by single-flight:42:x for 2400s".into());
        let mut rows = vec![mc];
        annotate(&mut rows, &facts(true));
        let mc = &rows[0];
        assert!(mc.failing);
        assert_eq!(mc.failing_for_s, Some(2400));
        assert!(mc.line.contains(" FAIL "), "{}", mc.line);
        assert!(mc.line.contains("cause=dead_holder"), "{}", mc.line);
        assert!(
            mc.line
                .contains("repair: fno agents claim release flight:k --holder single-flight:42:x"),
            "{}",
            mc.line
        );
        assert!(mc.line.ends_with("heal=auto"), "{}", mc.line);
        // A live holder leaves the held row alone.
        let mut live = row("merge_close", SCHED_DAEMON);
        live.skip_reason = Some("held".into());
        live.detail = Some("flight held by single-flight:42:x for 91s".into());
        let mut rows = vec![live];
        annotate(&mut rows.clone(), &facts(false));
        assert!(!rows[0].failing);
        assert!(rows[0].cause.is_none());
        // A dead holder inside the grace is the gate's to reclaim, not red.
        annotate(&mut rows, &facts(true));
        assert!(!rows[0].failing);
        assert!(rows[0].cause.is_none());
    }

    // AC3: an unknown failure keeps its skip token and names no verb.
    #[test]
    fn an_unknown_failure_reads_unclassified_operator() {
        let mut mc = row("merge_close", SCHED_DAEMON);
        mc.failing = true;
        mc.skip_reason = Some("error".into());
        mc.detail = Some("unreadable reconcile json".into());
        let mut rows = vec![mc];
        annotate(&mut rows, &facts(false));
        let mc = &rows[0];
        assert!(mc.line.contains("skip=error"), "{}", mc.line);
        assert!(mc.line.contains("cause=unclassified"), "{}", mc.line);
        assert!(mc.line.ends_with("heal=operator"), "{}", mc.line);
        assert!(!mc.line.contains("repair:"), "{}", mc.line);
        assert!(mc.repair.is_none());
    }

    #[test]
    fn a_not_a_repository_failure_names_the_node_repair() {
        let mut mc = row("merge_close", SCHED_DAEMON);
        mc.failing = true;
        mc.skip_reason = Some("failures".into());
        // The verbatim detail from the 2026-09-19 incident: the node was
        // filed from /private/tmp, so the reverse-map gh call ran there and
        // git refused.
        mc.detail = Some(
            "closed=0 promise_unmet=0 failures=2; first: x-cccc PR #0: reverse-map gh query \
             failed: gh pr list (merged) failed (rc=1): failed to run git: fatal: not a git \
             repository (or any of the parent directories)"
                .into(),
        );
        let mut rows = vec![mc];
        annotate(&mut rows, &facts(false));
        let mc = &rows[0];
        assert!(mc.line.contains("cause=cwd_not_checkout"), "{}", mc.line);
        assert!(
            mc.line
                .contains("repair: fno backlog update x-cccc --project <p> --cwd <checkout>"),
            "{}",
            mc.line
        );
        assert!(mc.line.ends_with("heal=operator"), "{}", mc.line);
    }

    #[test]
    fn an_unmeasured_selection_names_the_advance_repair() {
        let mut ac = row("auto_continue", SCHED_DAEMON);
        ac.failing = true;
        ac.skip_reason = Some("select-unmeasured".into());
        // Round-trip through the writer itself: select_read owns the detail
        // format, so a wording drift here fails this test instead of silently
        // degrading the repair verb.
        ac.detail = Some(crate::select_read::unmeasured_detail(
            crate::select_read::Kind::Next,
            &["--project".to_string(), "fno".to_string()],
            120,
            Some("selection stalled"),
        ));
        let mut rows = vec![ac];
        annotate(&mut rows, &facts(false));
        let ac = &rows[0];
        assert_eq!(ac.cause.as_deref(), Some("select_unmeasured"));
        assert_eq!(
            ac.repair.as_deref(),
            Some("fno backlog advance --project fno --source ac --json")
        );
        assert_eq!(ac.heal.as_deref(), Some("auto"));
        assert!(ac.line.contains("cause=select_unmeasured"), "{}", ac.line);
    }

    #[test]
    fn heal_retries_each_unmeasured_selection_once_per_timestamp() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        let mut ac = row("auto_continue", SCHED_DAEMON);
        ac.failing = true;
        ac.cause = Some("select_unmeasured".into());
        // Round-trip through the writer itself: select_read owns the detail
        // format, so a wording drift here fails this test instead of silently
        // degrading the repair verb.
        ac.detail = Some(crate::select_read::unmeasured_detail(
            crate::select_read::Kind::Next,
            &["--project".to_string(), "fno".to_string()],
            120,
            Some("selection stalled"),
        ));
        let rows = vec![ac];
        let mut runs = Vec::new();
        let first = heal(&rows, &[], true, &store, 0, 1800, &mut |action| {
            runs.push(action.to_string());
            true
        });
        assert_eq!(first, "heal=advance:fno:spawned");
        let second = heal(&rows, &[], true, &store, 300, 1800, &mut |action| {
            runs.push(action.to_string());
            true
        });
        assert_eq!(second, "heal=0");
        assert_eq!(runs, ["advance:fno"]);

        let mut next = rows.clone();
        next[0].last_ts = Some("2026-09-04T13:00:00Z".into());
        let third = heal(&next, &[], true, &store, 600, 1800, &mut |action| {
            runs.push(action.to_string());
            false
        });
        assert_eq!(third, "heal=advance:fno:failed");
        assert_eq!(runs, ["advance:fno", "advance:fno"]);
    }

    #[test]
    fn an_unnamed_project_repairs_without_a_project_flag() {
        let mut ac = row("auto_continue", SCHED_DAEMON);
        ac.failing = true;
        ac.skip_reason = Some("select-unmeasured".into());
        ac.detail = Some(crate::select_read::unmeasured_detail(
            crate::select_read::Kind::Next,
            &[],
            120,
            None,
        ));
        let mut rows = vec![ac];
        annotate(&mut rows, &facts(false));
        assert_eq!(
            rows[0].repair.as_deref(),
            Some("fno backlog advance --source ac --json")
        );
    }

    #[test]
    fn next_error_never_enters_the_advance_heal_lane() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        let mut ac = row("auto_continue", SCHED_DAEMON);
        ac.failing = true;
        ac.skip_reason = Some("next-error".into());
        ac.detail = Some("project=fno: graph unreadable".into());
        let mut rows = vec![ac];
        annotate(&mut rows, &facts(false));
        let mut runs = Vec::new();
        let token = heal(&rows, &[], true, &store, 0, 1800, &mut |action| {
            runs.push(action.to_string());
            true
        });
        assert_eq!(token, "heal=0");
        assert!(runs.is_empty());
    }

    // AC4: no pin, or a bad pin, is never a stale build.
    #[test]
    fn a_missing_or_bad_pin_is_no_evidence() {
        assert!(!install_off_main(None));
        assert!(!install_off_main(Some("not json")));
        assert!(!install_off_main(Some(
            r#"{"worktree_kind":"main_checkout","eligibility":"eligible"}"#
        )));
        assert!(!install_off_main(Some(
            r#"{"worktree_kind":"non_git","eligibility":"eligible"}"#
        )));
        assert!(install_off_main(Some(
            r#"{"worktree_kind":"linked_worktree","eligibility":"eligible"}"#
        )));
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.failing = true;
        kw.skip_reason = Some("timeout".into());
        let mut rows = vec![kw];
        annotate(&mut rows, &RepairFacts::new(install_off_main(None), &[]));
        assert_eq!(rows[0].cause.as_deref(), Some("timeout"));
        assert!(rows[0].line.contains("repair: fno do pr watch status"));
    }

    // AC2: a budget_spent row names the shortfall; a timeout row stops
    // blaming the step the clock happened to land in.
    #[test]
    fn budget_spent_row_names_the_count_and_timeout_names_the_clock() {
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.failing = true;
        kw.skip_reason = Some("budget_spent".into());
        kw.detail = Some(
            "crowns=5 evaluated=0/5 truth_reads=0 note=budget spent after 0 of 5 crowns".into(),
        );
        let mut rows = vec![kw];
        annotate(&mut rows, &RepairFacts::new(install_off_main(None), &[]));
        assert_eq!(rows[0].cause.as_deref(), Some("budget_spent"));
        assert!(rows[0].line.contains("repair: fno do pr watch status"));
        assert!(
            rows[0]
                .line
                .contains("ran out of its slice before it covered every unit it enumerated"),
            "line: {}",
            rows[0].line
        );

        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.failing = true;
        kw.skip_reason = Some("timeout".into());
        kw.detail = Some("phase slice 45s spent at king_wake:mail".into());
        let mut rows = vec![kw];
        annotate(&mut rows, &RepairFacts::new(install_off_main(None), &[]));
        assert_eq!(rows[0].cause.as_deref(), Some("timeout"));
        assert!(
            rows[0]
                .line
                .contains("the step named in the detail is where the clock stopped"),
            "line: {}",
            rows[0].line
        );
    }

    #[test]
    fn a_stale_cause_keeps_its_hint_and_gains_repair_and_owner() {
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.stale = true;
        kw.cause = Some("tick_overdue".into());
        kw.line = "king_wake STALE cause=tick_overdue (evidence)".into();
        let mut rows = vec![kw];
        annotate(&mut rows, &facts(false));
        assert_eq!(
            rows[0].line,
            "king_wake STALE cause=tick_overdue (evidence) repair: fno do pr watch refresh heal=auto"
        );
        // Configured off is not a fault: no suffix.
        let mut off = row("watchdog", SCHED_LAUNCHD);
        off.stale = true;
        off.cause = Some("configured_off".into());
        off.line = "watchdog STALE cause=configured_off (x)".into();
        let mut rows = vec![off];
        annotate(&mut rows, &facts(false));
        assert_eq!(rows[0].line, "watchdog STALE cause=configured_off (x)");
    }

    // AC11 core: a dead hold is released by one heal pass.
    #[test]
    fn heal_releases_a_dead_hold() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        write_claim(td.path(), "flight:probe", "single-flight:1:a", dead_pid());
        let findings = vec![holder_finding(
            td.path(),
            "flight:probe",
            "single-flight:1:a",
        )];
        let token = heal(&[], &findings, true, &store, 0, 1800, &mut |_| {
            panic!("no run")
        });
        assert_eq!(token, "heal=dead_holder:1");
        let (_, rec) = crate::claims::status("flight:probe", Some(td.path()));
        assert!(rec.is_none(), "the dead hold is gone");
    }

    // AC12: a claim re-acquired by a live holder between the read and the release stays.
    #[test]
    fn heal_never_drops_a_live_replacement() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        write_claim(
            td.path(),
            "flight:probe",
            "single-flight:2:live",
            std::process::id() as i32,
        );
        let findings = vec![holder_finding(
            td.path(),
            "flight:probe",
            "single-flight:1:a",
        )];
        let token = heal(&[], &findings, true, &store, 0, 1800, &mut |_| {
            panic!("no run")
        });
        assert_eq!(token, "heal=skipped:1");
        let (_, rec) = crate::claims::status("flight:probe", Some(td.path()));
        assert_eq!(rec.unwrap().holder, "single-flight:2:live");
    }

    // AC13: the switch off runs nothing.
    #[test]
    fn heal_off_runs_nothing() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        write_claim(td.path(), "flight:probe", "single-flight:1:a", dead_pid());
        let findings = vec![holder_finding(
            td.path(),
            "flight:probe",
            "single-flight:1:a",
        )];
        let token = heal(&[], &findings, false, &store, 0, 1800, &mut |_| {
            panic!("no run")
        });
        assert_eq!(token, "heal=off");
        let (_, rec) = crate::claims::status("flight:probe", Some(td.path()));
        assert!(rec.is_some());
    }

    // AC14: one refresh per stale episode; a new episode refreshes again.
    #[test]
    fn heal_refreshes_once_per_episode() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.stale = true;
        kw.cause = Some("tick_overdue".into());
        kw.age_s = Some(2400);
        let rows = vec![kw];
        let mut runs = Vec::new();
        let first = heal(&rows, &[], true, &store, 0, 1800, &mut |a| {
            runs.push(a.to_string());
            true
        });
        assert_eq!(first, "heal=refresh:ok");
        let second = heal(&rows, &[], true, &store, 300, 1800, &mut |a| {
            runs.push(a.to_string());
            true
        });
        assert_eq!(second, "heal=0");
        assert_eq!(runs, ["refresh"]);
        let mut next = rows.clone();
        next[0].last_ts = Some("2026-09-04T13:00:00Z".into());
        let third = heal(&next, &[], true, &store, 600, 1800, &mut |_| false);
        assert_eq!(third, "heal=refresh:failed");
    }

    #[test]
    fn heal_installs_at_most_once_per_six_hours() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.failing = true;
        kw.cause = Some("stale_build".into());
        let rows = vec![kw];
        let mut runs = 0;
        let now = 1_788_523_200;
        for tick in 0..3 {
            heal(&rows, &[], true, &store, now + tick * 300, 1800, &mut |a| {
                assert_eq!(a, "install");
                runs += 1;
                true
            });
        }
        assert_eq!(runs, 1);
        heal(&rows, &[], true, &store, now + 21_600, 1800, &mut |_| {
            runs += 1;
            true
        });
        assert_eq!(runs, 2);
    }

    // AC5-HP: an annotated paused row ends in a read verb, never a refresh,
    // and carries no heal owner.
    #[test]
    fn a_paused_row_reads_the_incident_verb_and_no_heal() {
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.cause = Some("fleet_stop".into());
        kw.line = format!(
            "{} cause=fleet_stop (fleet incident stopped at generation 5: two cargo runs; \
             held on purpose; wait for the breaker to clear)",
            render_row(&kw)
        );
        let mut rows = vec![kw];
        annotate(&mut rows, &facts(false));
        let kw = &rows[0];
        assert!(
            kw.line.ends_with("repair: fno agents incident status"),
            "line: {}",
            kw.line
        );
        assert_eq!(kw.repair.as_deref(), Some("fno agents incident status"));
        assert_eq!(kw.heal, None);
        assert!(!kw.line.contains("pr watch refresh"), "line: {}", kw.line);
    }

    // AC6-HP: self-heal runs nothing under a breaker and says why.
    #[test]
    fn heal_runs_nothing_under_a_pause() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path().join("signals.json");
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.cause = Some("fleet_stop".into());
        kw.age_s = Some(3000);
        let rows = vec![kw];
        let mut calls: Vec<String> = Vec::new();
        let token = heal(&rows, &[], true, &store, 600, 1800, &mut |a| {
            calls.push(a.to_string());
            true
        });
        assert_eq!(token, "heal=paused");
        assert!(calls.is_empty(), "runs: {calls:?}");
    }

    // AC7-EDGE: a hand-pause names the loops verb.
    #[test]
    fn a_hand_paused_row_names_the_loops_verb() {
        let mut kw = row("king_wake", SCHED_LAUNCHD);
        kw.cause = Some("loops_paused".into());
        kw.line = format!(
            "{} cause=loops_paused (loops paused by hand; \
             held on purpose; wait for resume-all)",
            render_row(&kw)
        );
        let mut rows = vec![kw];
        annotate(&mut rows, &facts(false));
        let kw = &rows[0];
        assert!(
            kw.line.ends_with("repair: fno do loops status"),
            "line: {}",
            kw.line
        );
        assert_eq!(kw.heal, None);
    }
}

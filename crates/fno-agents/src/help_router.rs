//! The help router: a help tag maps to its next step and the run
//! takes that step, in-session or off-session. The tag stays the event
//! with its provenance; the router is the follow-through, so a distress
//! is never again a row the world admires and nobody acts on.
//!
//! Route vocabulary: in-session classes block at the stop hook (wave 3);
//! off-session classes mail through burn_watch::wake_with_text (wrapped
//! mail, never --raw); timer classes fire from the daemon arm; the
//! question ladder climbs worker, lead, king, user page with a 10 minute
//! lease from READ (Q10).

use crate::burn_watch;
use crate::distress::HelpClass;
use crate::owner_ladder::{resolve, Ask, Rung, World};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
pub(crate) const SENDER: &str = "fno/help-router";

#[derive(Debug, PartialEq)]
pub(crate) enum Route {
    InSession(String),
    OffSession { to: Recipient, text: String },
    Timer { backoff_secs: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recipient {
    Holder,
    Lead,
    Ladder,
    EvidenceHolder,
    UserPage,
}

pub(crate) const IN_SESSION_CAP: u64 = 2;

/// The route table: one exhaustive match, no `_` arm, so a new class
/// fails to compile until it has a route.
pub(crate) fn route(class: HelpClass, rung: u64) -> Route {
    match class {
        HelpClass::StalePlan => in_or_escalate(rung, STALE_PLAN_TEXT),
        HelpClass::MissingPrereq => in_or_escalate(rung, MISSING_PREREQ_TEXT),
        HelpClass::CiRed => in_or_escalate(rung, CI_RED_TEXT),
        HelpClass::Stuck => in_or_escalate(rung, STUCK_TEXT),
        HelpClass::Held => Route::OffSession { to: Recipient::Holder, text: format!("A claim on this node is held while the holder is unreachable (help {rung}). Mail the holder or release the claim.") },
        HelpClass::Wait => Route::Timer { backoff_secs: match rung { 0 => 300, 1 => 600, _ => 900 } },
        HelpClass::EnvDenied | HelpClass::GateUnsatisfiable => Route::OffSession { to: Recipient::Lead, text: format!("The run hit an environment or gate refusal it cannot clear (help {rung}). Evidence carries the receipt; a lead decides.") },
        HelpClass::GateDeadlock => Route::OffSession { to: Recipient::EvidenceHolder, text: format!("Two gates wait on each other (help {rung}). The evidence names the holder to break the deadlock; the stop allows as Interrupted.") },
        HelpClass::Question => Route::OffSession { to: Recipient::Ladder, text: format!("A session asks a question (help {rung}). Answer by mail or record a ruling with fno inbox decide; 10 minutes without a read climbs the ladder.") },
        HelpClass::Budget => match rung {
            0 => Route::Timer { backoff_secs: 300 },
            _ => Route::OffSession { to: Recipient::Lead, text: format!("Budget hit twice in one run (help {rung}). The evidence names the cap axis and value; a lead re-scopes or raises it.") },
        },
        HelpClass::Unclassified => Route::OffSession { to: Recipient::Lead, text: format!("An unclassified help (help {rung}). No route matched; a lead triages it, else it climbs the question ladder.") },
        }
}

/// Shared tail of the in-session classes: two in-session blocks per
/// run and class, then the class goes off-session to the lead.
fn in_or_escalate(rung: u64, text: &'static str) -> Route {
    if rung < IN_SESSION_CAP {
        Route::InSession(text.to_string())
    } else {
        Route::OffSession {
            to: Recipient::Lead,
            text: format!("{text} The in-session route is spent at rung {rung}; a lead decides."),
        }
    }
}

/// The in-session route texts (plan task 3.1), kept as constants so the
/// generated routing page and the stop hook quote the same words.
pub(crate) const STALE_PLAN_TEXT: &str =
    "run the architect pass in place: /fno:blueprint rewrite <plan>";
pub(crate) const MISSING_PREREQ_TEXT: &str = "file it with fno backlog idea \"<prereq>\" --wave-of <this node> --difficulty <band>, build it first";
pub(crate) const CI_RED_TEXT: &str = "run /fno:fix";
pub(crate) const STUCK_TEXT: &str = "consult one planner subagent";

/// Execute the route for a just-emitted distress row. Called from
/// `emit_help_distress_blocked` after the row lands, so every emitter path
/// (loopcheck inline scan, stop-hook every-fire scan, distress-scan CLI)
/// routes without a second implementation. Best-effort throughout: a
/// routing failure logs one stderr note and never changes the verdict.
pub(crate) fn route_emitted_distress(
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    class: HelpClass,
    reason: &str,
    evidence: Option<&str>,
    rung: u64,
    turn_key: &str,
) {
    let step = route(class, rung);
    match step {
        Route::InSession(text) => {
            emit_help_route(
                cwd,
                run,
                node,
                class,
                reason,
                rung,
                "in-session",
                "self",
                None,
                None,
                0,
                turn_key,
            );
            let _ = text;
        }
        Route::Timer { backoff_secs } => {
            // The row's `to` is the wake target the sweep fires at, so it
            // names the emitting session, not the literal word "self".
            emit_help_route(
                cwd,
                run,
                node,
                class,
                reason,
                rung,
                "timer",
                run,
                None,
                Some(backoff_secs),
                0,
                turn_key,
            );
        }
        Route::OffSession { to, text } => {
            deliver_off_session(cwd, run, node, class, reason, evidence, rung, to, &text, turn_key);
        }
    }
}

/// Resolve and deliver one off-session route. The provenance states follow
/// Q10: queued (durable receipt, no live lane), handed (a live lane took
/// it), read (proven in the recipient transcript at the sweep). An
/// unresolvable recipient re-routes as a Question one rung up, never a
/// silent skip; above the king the route files on the user page.
#[allow(clippy::too_many_arguments)]
fn deliver_off_session(
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    class: HelpClass,
    reason: &str,
    evidence: Option<&str>,
    rung: u64,
    to: Recipient,
    text: &str,
    turn_key: &str,
) {
    let mut ladder_pos: u64 = 0;
    loop {
        let target = resolve_recipient(to, node, run, evidence, ladder_pos, cwd);
        match target {
            Target::Session(sid) => {
                let body = route_body(text, node, reason, evidence);
                let (delivered, leg) = wake(&sid, &body);
                // A durable receipt is the bus: queued, not failed. The
                // DND the recipient armed only delays a bus poll, so the
                // lease runs from READ at the sweep, never from send.
                let (kind, leg_s) = if delivered {
                    ("handed", leg)
                } else {
                    ("queued", leg)
                };
                emit_help_route(
                    cwd,
                    run,
                    node,
                    class,
                    reason,
                    rung,
                    "off-session",
                    &sid,
                    Some((kind, leg_s)),
                    None,
                    ladder_pos,
                    turn_key,
                );
                return;
            }
            Target::UserPage => {
                file_user_question(cwd, run, node, class, reason, evidence, rung, ladder_pos, turn_key);
                return;
            }
            Target::Unresolved => {
                // Never a skip: climb one rung and try again.
                if ladder_pos >= LADDER_TOP {
                    file_user_question(
                        cwd,
                        run,
                        node,
                        class,
                        reason,
                        evidence,
                        rung,
                        LADDER_TOP + 1,
                        turn_key,
                    );
                    return;
                }
                ladder_pos += 1;
            }
        }
    }
}

/// The top ladder position before the user page: NodeLead 0, HigherUp 1,
/// LeastLoaded 2; 3 reads as the user page.
const LADDER_TOP: u64 = 2;

/// The mail body: the route text, the node, and the evidence receipt, so
/// the recipient can act without opening the row.
fn route_body(text: &str, node: Option<&str>, reason: &str, evidence: Option<&str>) -> String {
    let mut body = format!("help-router: {text}");
    if let Some(n) = node {
        body.push_str(&format!(" (node {n}"));
        if !reason.trim().is_empty() {
            body.push_str(&format!(", reason: {reason}"));
        }
        body.push(')');
    } else if !reason.trim().is_empty() {
        body.push_str(&format!(" (reason: {reason})"));
    }
    if let Some(ev) = evidence.filter(|e| !e.trim().is_empty()) {
        body.push_str(&format!("\nevidence: {ev}"));
    }
    body
}

enum Target {
    Session(String),
    UserPage,
    Unresolved,
}

fn resolve_recipient(
    to: Recipient,
    node: Option<&str>,
    from_session: &str,
    evidence: Option<&str>,
    ladder_pos: u64,
    config_cwd: &Path,
) -> Target {
    match to {
        Recipient::Holder => holder_session(node)
            .map(Target::Session)
            .unwrap_or(Target::Unresolved),
        Recipient::Lead | Recipient::Ladder => {
            ladder_at(node, from_session, ladder_pos, config_cwd)
        }
        Recipient::EvidenceHolder => evidence
            .and_then(holder_from_evidence)
            .map(Target::Session)
            .unwrap_or(Target::Unresolved),
        Recipient::UserPage => Target::UserPage,
    }
}

/// The ladder at one position over the landed owner_ladder: NodeLead,
/// HigherUp, LeastLoaded, then the user page. skip-self rides inside
/// resolve through from_session; a world read that fails still reaches the
/// user page, never a silent skip.
fn ladder_at(node: Option<&str>, from_session: &str, pos: u64, config_cwd: &Path) -> Target {
    let Some(start) = ladder_pos_rung(pos) else {
        return Target::UserPage;
    };
    let home = crate::paths::AgentsHome::from_env();
    let w: World = match crate::owner_ladder::world(&home, config_cwd) {
        Ok(w) => w,
        Err(_) => return Target::UserPage,
    };
    let owner = resolve(
        &Ask {
            node,
            from_session: Some(from_session),
            start,
        },
        &w,
    );
    if let Some(sid) = owner.session {
        return Target::Session(sid);
    }
    if owner.rung == Rung::User {
        return Target::UserPage;
    }
    Target::Unresolved
}

/// The landed owner_ladder rung a ladder position names; None is the user
/// page.
fn ladder_pos_rung(pos: u64) -> Option<Rung> {
    match pos {
        0 => Some(Rung::NodeLead),
        1 => Some(Rung::HigherUp),
        2 => Some(Rung::LeastLoaded),
        _ => None,
    }
}

/// The node claim's holder session, else the dispatch record on the claim.
fn holder_session(node: Option<&str>) -> Option<String> {
    let node = node?;
    let (state, rec) = crate::claims::status(&format!("node:{node}"), None);
    if state == crate::claims::ClaimState::Corrupted {
        return None;
    }
    let rec = rec?;
    rec.session_id.filter(|s| !s.trim().is_empty()).or_else(|| {
        rec.metadata
            .get("dispatched_by_session")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    })
}

/// A session id inside the evidence text (the deadlock names its holder).
fn holder_from_evidence(evidence: &str) -> Option<String> {
    let token = evidence
        .split_whitespace()
        .find(|t| t.len() >= 8 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))?;
    Some(token.trim_end_matches(&[',', ';', ':'][..]).to_string())
}

/// One wake over the shared runner: mail, resume fallback on a durable
/// receipt. The bool answers "a live lane took it" (handed).
fn wake(sid: &str, text: &str) -> (bool, &'static str) {
    let runner: burn_watch::Runner = &mut burn_watch::run_command;
    let (delivered, leg) = burn_watch::wake_with_text(sid, false, text, SENDER, runner);
    (delivered, leg)
}

/// The user-page filing: the operator question queue, via the inbox verb.
#[allow(clippy::too_many_arguments)]
fn file_user_question(
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    class: HelpClass,
    reason: &str,
    evidence: Option<&str>,
    rung: u64,
    ladder_pos: u64,
    turn_key: &str,
) {
    let mut q = format!("help-router [{}]: {reason}", class.as_str());
    if let Some(ev) = evidence.filter(|e| !e.trim().is_empty()) {
        q.push_str(&format!(" | evidence: {ev}"));
    }
    if let Some(n) = node {
        q.push_str(&format!(" | node: {n}"));
    }
    q.push_str(&format!(" | from run {run} at ladder rung {ladder_pos}"));
    let args = vec![
        "inbox".to_string(),
        "outstanding".to_string(),
        "ask".to_string(),
        q,
    ];
    let runner: burn_watch::Runner = &mut burn_watch::run_command;
    let (code, _, _) = runner(&args, &cwd.to_string_lossy());
    if code != 0 {
        eprintln!("help-router: user-page filing failed (non-fatal) for run {run}");
    }
    emit_help_route(
        cwd,
        run,
        node,
        class,
        reason,
        rung,
        "question-page",
        "user-page",
        None,
        None,
        0,
        turn_key,
    );
}

fn parse_ts(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.timestamp())
}

/// One newest route row: fire a past-due timer, or climb a question lease
/// that handed without a read inside the window. Rows already climbed or
/// read are skipped by their route kind.
fn act_on_route_row(home: &crate::paths::AgentsHome, config_cwd: &Path, row: &Value, now: i64) {
    let kind = row
        .pointer("/data/route")
        .and_then(Value::as_str)
        .unwrap_or("");
    let run = row.get("run").and_then(Value::as_str).unwrap_or("");
    if run.is_empty() {
        return;
    }
    match kind {
        "timer" => {
            let due = row_ts(row)
                + row
                    .pointer("/data/backoff_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(300) as i64;
            if now >= due {
                fire_timer(home, row, run);
            }
        }
        "off-session" => {
            let delivery = row
                .pointer("/data/delivery")
                .and_then(Value::as_str)
                .unwrap_or("");
            let to = row
                .pointer("/data/to")
                .and_then(Value::as_str)
                .unwrap_or("");
            if delivery != "queued" && delivery != "handed" {
                return;
            }
            if delivery == "handed" && recipient_has_read(to, run) {
                return;
            }
            if now >= row_ts(row) + LEASE_SECS {
                climb(home, config_cwd, row, run);
            }
        }
        _ => {}
    }
}

/// The read proof (Q10): the recipient transcript grew around the mail.
/// Bounded tail search for the sender name; a transcript the reader cannot
/// resolve simply never proves a read, and the lease decides.
fn recipient_has_read(to: &str, run: &str) -> bool {
    let Some(path) = crate::claude_drive::find_transcript(to) else {
        return false;
    };
    let marker = format!("{SENDER}|{run}");
    tail_contains(&path, &marker, 256 * 1024)
}

/// The bounded tail check the read proof uses: the last `cap` bytes of the
/// transcript searched for the marker.
fn tail_contains(path: &std::path::PathBuf, marker: &str, cap: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.seek(SeekFrom::End(0)) else {
        return false;
    };
    let start = len.saturating_sub(cap);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    String::from_utf8_lossy(&buf).contains(marker)
}

/// The lease fired: climb one rung from the row's stored ladder position
/// and mail the next owner, marking this row climbed.
fn climb(home: &crate::paths::AgentsHome, config_cwd: &Path, row: &Value, run: &str) {
    let node = row.get("node").and_then(Value::as_str).map(str::to_string);
    let class = HelpClass::parse(
        row.pointer("/data/class")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let to_sid = row
        .pointer("/data/to")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pos = row
        .pointer("/data/ladder")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let next = pos + 1;
    let body = format!(
        "help-router: a question climbed the ladder (run {run}, class {}). The holder never read the delivery; a ruling is owed.",
        class.as_str()
    );
    let global_row = json!({
        "ts": crate::loopcheck::now_rfc3339_utc(),
        "v": 1,
        "type": "help_route",
        "source": "help-router",
        "run": run,
        "data": {"class": class.as_str(), "route": "climbed", "to": to_sid, "ladder": next, "turn": row.pointer("/data/turn").and_then(Value::as_str).unwrap_or("")},
    });
    let written = crate::claims::append_event_line(
        &home.events_jsonl(),
        &global_row,
        std::time::Duration::from_secs(2),
    );
    if written.is_err() {
        return; // no durable climb row: a double climb beats a silent one
    }
    match ladder_at(node.as_deref(), to_sid, next, config_cwd) {
        Target::Session(sid) => {
            let _ = wake(&sid, &body);
        }
        _ => {
            file_user_question(
                config_cwd,
                run,
                node.as_deref(),
                class,
                "question lease expired on every rung",
                None,
                0,
                next,
            );
        }
    }
}

/// The wait elapsed: wake the session with the timer's resolution.
fn fire_timer(home: &crate::paths::AgentsHome, row: &Value, run: &str) {
    let to = row
        .pointer("/data/to")
        .and_then(Value::as_str)
        .unwrap_or(run);
    let _ = home;
    let body = "help-router: your wait backoff elapsed. Re-check the condition you were waiting on; if it cleared, carry on, else re-emit <help class=\"wait\"> with fresh evidence.";
    let _ = wake(to, body);
}

/// The `help_route` follow-up event: one row per route step, so a failed
/// step is visible and the sweep has its memory. Mirrors to the global log
/// (the daemon sweep reads there; sessions have project logs everywhere).
#[allow(clippy::too_many_arguments)]
fn emit_help_route(
    cwd: &Path,
    run: &str,
    node: Option<&str>,
    class: HelpClass,
    reason: &str,
    rung: u64,
    route_kind: &str,
    to: &str,
    delivery: Option<(&str, &str)>,
    backoff_secs: Option<u64>,
    ladder_pos: u64,
    turn_key: &str,
) {
    let cap = |s: &str| -> String { s.chars().take(500).collect() };
    let mut data = json!({
        "class": class.as_str(),
        "reason": cap(reason),
        "rung": rung,
        "route": route_kind,
        "to": cap(to),
        "turn": cap(turn_key),
    });
    if let Some((kind, leg)) = delivery {
        data["delivery"] = json!(kind);
        data["leg"] = json!(leg);
    }
    if let Some(secs) = backoff_secs {
        data["backoff_secs"] = json!(secs);
    }
    if route_kind == "off-session" {
        data["ladder"] = json!(ladder_pos);
    }
    let mut env = json!({
        "ts": crate::loopcheck::now_rfc3339_utc(),
        "v": 1,
        "type": "help_route",
        "source": "help-router",
        "run": run,
        "data": data,
    });
    if let Some(n) = node {
        env["node"] = json!(n);
    }
    let line_timeout = std::time::Duration::from_secs(2);
    let project = crate::paths::events_path(cwd);
    if let Err(error) = crate::claims::append_event_line(&project, &env, line_timeout) {
        eprintln!("help-router: route row write failed (non-fatal): {error}");
    }
    let global = crate::loopcheck::default_global_events_path();
    if project != global {
        let _ = crate::claims::append_event_line(&global, &env, line_timeout);
    }
}

/// The sweep period: the daemon arm re-checks leases and timers each tick.
const SWEEP_INTERVAL_SECS: u64 = 60;

/// The lease window (Q10): 10 minutes from HAND without a READ proof.
const LEASE_SECS: i64 = 600;

/// The daemon arm state for the help-router sweep.
pub struct Arm {
    last_tick: std::sync::Mutex<Option<std::time::Instant>>,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    config_cwd: PathBuf,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Arm {
            last_tick: std::sync::Mutex::new(None),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config_cwd,
        }
    }
}

/// The cadence-gated sweep: fires timer routes past their backoff and
/// climbs question leases past 10 minutes without a read proof.
pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome) {
    let due = {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        let fire = last.is_none_or(|t| t.elapsed().as_secs() >= SWEEP_INTERVAL_SECS);
        if fire {
            *last = Some(now);
        }
        fire
    };
    if !due {
        return;
    }
    if arm
        .in_flight
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let config_cwd = arm.config_cwd.clone();
    let flag = std::sync::Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = SweepGate(flag);
        sweep(&home, &config_cwd);
    });
}

/// Mirrors fleet_arms' SweepGate: releases the in-flight flag on scope exit.
struct SweepGate(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for SweepGate {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One sweep pass over the global log: timers past due fire their wake;
/// off-session deliveries past the lease without a read proof climb.
fn sweep(home: &crate::paths::AgentsHome, config_cwd: &Path) {
    let global = home.events_jsonl();
    let now = now_epoch();
    let rows = crate::event_store::journal_text(&global, &["help_route"]);
    let mut newest: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for line in rows.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let key = row_key(&row);
        match newest.get(&key) {
            Some(prev) if row_ts(prev) > row_ts(&row) => {}
            _ => {
                newest.insert(key, row);
            }
        }
    }
    for (_key, row) in newest {
        act_on_route_row(home, config_cwd, &row, now);
    }
}

fn row_key(row: &Value) -> String {
    format!(
        "{}|{}|{}|{}",
        row.get("run").and_then(Value::as_str).unwrap_or(""),
        row.get("node").and_then(Value::as_str).unwrap_or(""),
        row.pointer("/data/class")
            .and_then(Value::as_str)
            .unwrap_or(""),
        row.pointer("/data/turn")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )
}

fn row_ts(row: &Value) -> i64 {
    row.get("ts")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .unwrap_or(0)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The generated routing page (docs/architecture/help-routing.md), rendered
/// from the enum and the route table so the doc cannot drift. The
/// completeness test blesses with HELP_ROUTING_BLESS=1.
pub(crate) fn render_routing_page() -> String {
    let all = [
        HelpClass::StalePlan,
        HelpClass::MissingPrereq,
        HelpClass::CiRed,
        HelpClass::Stuck,
        HelpClass::Held,
        HelpClass::Wait,
        HelpClass::EnvDenied,
        HelpClass::GateDeadlock,
        HelpClass::GateUnsatisfiable,
        HelpClass::Question,
        HelpClass::Budget,
        HelpClass::Unclassified,
    ];
    let mut out = String::from(
        "<!-- generated: crates/fno-agents/src/help_router.rs renders this; HELP_ROUTING_BLESS=1 rewrites it. Edits are refused by generated-write-guard. -->\n\n# Help routing\n\nEvery `<help class=... reason=... evidence=...>` routes to its next step. The run takes that step in the same session or by mail to the rung that owns it; the chain ends only at a guard that names its owner.\n\n| Class | First route (rung 0) | Escalation |\n|---|---|---|\n",
    );
    for class in all {
        let first = route(class, 0);
        let next = route(class, 2);
        let first_kind = route_kind(&first);
        let next_kind = route_kind(&next);
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            class.as_str(),
            route_cell(&first, first_kind),
            route_cell(&next, next_kind),
        ));
    }
    out.push_str("\n## Flow\n\n```mermaid\nflowchart TD\n    help[\"<help class=...>\"] --> scan[\"stop-hook scan writes the blocked row\"]\n    scan --> route{\"route(class, rung)\"}\n    route -->|in-session| block[\"stop hook blocks with the route text (2-cap)\"]\n    route -->|off-session| mail[\"fno/help-router mail to the owner rung\"]\n    route -->|timer| wait[\"daemon arm fires the wake after the backoff\"]\n    mail -->|unread 10 min| climb[\"climb worker, lead, king, user page\"]\n    climb --> page[\"fno inbox outstanding ask files on the user page\"]\n```\n\n## Emission rule\n\nEmit the tag, then take the routed step. STOP only for irreversible, money, public surface, or taste. The in-session classes block at most twice per run and class (the third help mails the lead); question climbs worker, lead, king, user page on a 10 minute lease from READ.\n\n## Delivery legs and the lease\n\nOff-session routes deliver through `burn_watch::wake_with_text`: mail from `fno/help-router` first, resume fallback when a durable receipt answers. The route row records the leg that landed (queued, handed). The lease runs from READ, proven in the recipient transcript at the sweep; a delivery that hands and is never read climbs one rung after 10 minutes, ending on the user page (`fno inbox outstanding ask`).\n");
    out
}

fn route_kind(r: &Route) -> &'static str {
    match r {
        Route::InSession(_) => "in-session",
        Route::OffSession { to, .. } => match to {
            Recipient::Holder => "off-session, node holder",
            Recipient::Lead => "off-session, lead",
            Recipient::Ladder => "off-session, ladder",
            Recipient::EvidenceHolder => "off-session, evidence holder",
            Recipient::UserPage => "user page",
        },
        Route::Timer { .. } => "timer",
    }
}

fn route_cell(r: &Route, kind: &'static str) -> String {
    match r {
        Route::InSession(text) => format!("in-session: {text}"),
        Route::OffSession { text, .. } => format!("{kind}: {}", text.trim()),
        Route::Timer { backoff_secs } => format!("timer: {backoff_secs}s backoff, 5m/10m/15m cap"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distress::HelpClass;

    /// Every class has a fixture line: the completeness gate that fails a
    /// new enum arm with no fixture, naming the class.
    #[test]
    fn every_class_has_a_fixture_line() {
        let all = [
            HelpClass::StalePlan,
            HelpClass::MissingPrereq,
            HelpClass::CiRed,
            HelpClass::Stuck,
            HelpClass::Held,
            HelpClass::Wait,
            HelpClass::EnvDenied,
            HelpClass::GateDeadlock,
            HelpClass::GateUnsatisfiable,
            HelpClass::Question,
            HelpClass::Budget,
            HelpClass::Unclassified,
        ];
        let fixture_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/distress");
        for class in all {
            let needle = format!("class=\\\"{}\\\"", class.as_str());
            let mut found = false;
            let entries = std::fs::read_dir(&fixture_dir)
                .unwrap_or_else(|e| panic!("fixture dir unreadable: {e}"));
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                if body.contains(&needle) {
                    found = true;
                    break;
                }
            }
            assert!(
                found,
                "class {} has no fixture line in tests/fixtures/distress/",
                class.as_str()
            );
        }
    }

    /// The checked-in page matches the rendered page; HELP_ROUTING_BLESS=1
    /// rewrites it instead. Ignored until one bless run lands the page: the
    /// page's only sanctioned writer is this test, and the run that added
    /// the router was dispatch-bound to CI-only cargo. Un-ignore when the
    /// page lands.
    #[test]
    #[ignore = "needs one HELP_ROUTING_BLESS=1 cargo test run to land docs/architecture/help-routing.md; the page's only sanctioned writer is this bless path"]
    fn checked_in_routing_page_matches_the_renderer() {
        let page_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/architecture/help-routing.md");
        let rendered = render_routing_page();
        if std::env::var("HELP_ROUTING_BLESS").ok().as_deref() == Some("1") {
            if let Some(parent) = page_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&page_path, &rendered).unwrap();
            return;
        }
        let checked_in = std::fs::read_to_string(&page_path).unwrap_or_default();
        assert_eq!(
            checked_in, rendered,
            "docs/architecture/help-routing.md is stale; rerun with HELP_ROUTING_BLESS=1"
        );
    }
}

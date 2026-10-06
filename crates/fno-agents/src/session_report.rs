//! Session-start reports: what each harness session started as, into the
//! registry.
//!
//! One thin per-harness hook (`hooks/register-session-start.sh` and its
//! per-harness siblings) posts its RAW SessionStart payload to the daemon via
//! `fno-agents report --kind session`; this module is both ends. The daemon side
//! stamps the matching registry row additively - an empty primary session id
//! fills (and promotes `spawning` to `live`), a second different id fills the
//! ONE optional related slot, a third distinct id refuses the write - and
//! records the transcript path + start source the harness itself named, so
//! mail and liveness stop guessing them from transcripts. The client side
//! never lazy-starts a daemon and spools to a capped file when the daemon is
//! down, so a session start never blocks and never loses its report.
//!
//! Same session-id matching as the inside-leg report (`handle_report`):
//! `entry_holds_session`, then the claude short-id backfill, then the row
//! NAME when the hook carries `FNO_AGENT_SELF` - the name is the durable key
//! a resumed worker corrects on, because the spawn-time id is not.

use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::protocol::{ErrorCode, Request, Response};
use crate::state::{self, RegistryEntry};
use crate::AgentStatus;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// Hard cap on the wait for a not-yet-written row (`--wait-row`): a pane
/// substrate's row is written by the spawner AFTER `mux pane run` returns, so
/// a fast-booting child can fire its SessionStart hook first. Same bound as
/// the retired Python restamp wait (`register_session._RESTAMP_ROW_WAIT_S`).
const WAIT_ROW_CAP: Duration = Duration::from_secs(10);
const WAIT_ROW_POLL: Duration = Duration::from_millis(250);
/// Same deadline as the inside-leg report's REPORT_TIMEOUT: a hook report to
/// an ALREADY-RUNNING daemon answers in milliseconds; anything longer is a
/// lost report, not a blocked session start.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Spool cap: frames, oldest dropped. A session-start fire is a few hundred
/// bytes, so this bounds the file well under a megabyte.
const SPOOL_CAP_FRAMES: usize = 128;
/// Raw hook payloads are small; refuse to read more off stdin.
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// daemon side: agent.session_report

fn str_field(p: &Map<String, Value>, key: &str) -> Option<String> {
    p.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn payload_field<'a>(p: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    p.get("payload")
        .and_then(Value::as_object)
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Which row a report lands on, and what the match means for the id.
enum Find {
    /// `entry_holds_session` already answers true: the row records this id.
    Holds(usize),
    /// The claude short-id backfill match: a bg row awaiting its full uuid.
    Backfill(usize),
    /// The named row (`FNO_AGENT_SELF`): the durable key a resumed worker
    /// corrects on.
    Named(usize),
}

fn find_row(
    entries: &[RegistryEntry],
    harness: &str,
    session_id: &str,
    agent_self: Option<&str>,
) -> Option<Find> {
    if let Some(i) = entries
        .iter()
        .position(|e| crate::daemon::entry_holds_session(e, session_id))
    {
        return Some(Find::Holds(i));
    }
    if harness == "claude" || harness == "codex" {
        match crate::daemon::find_uuid_backfill_row(entries, session_id) {
            crate::daemon::UuidBackfill::One(i) => return Some(Find::Backfill(i)),
            crate::daemon::UuidBackfill::Ambiguous => return None,
            crate::daemon::UuidBackfill::None => {}
        }
    }
    let agent_self = agent_self?;
    let i = entries.iter().position(|e| {
        e.harness_name() == harness
            && (e.name == agent_self || e.aliases.iter().any(|a| a == agent_self))
    })?;
    Some(Find::Named(i))
}

enum Outcome {
    Stored { related_filled: bool },
    Unknown,
    IdCap { primary: String, related: String },
}

/// Poll for a row that does not exist yet (pane race) before taking the write
/// lock. Plain reads; the write closure re-decides everything under the lock.
fn wait_for_row(
    path: &Path,
    harness: &str,
    session_id: &str,
    agent_self: Option<&str>,
    wait: Duration,
) {
    let deadline = Instant::now() + wait.min(WAIT_ROW_CAP);
    while Instant::now() < deadline {
        if let Ok(reg) = crate::daemon::load_registry_asserted(path) {
            if find_row(&reg.entries, harness, session_id, agent_self).is_some() {
                return;
            }
        }
        std::thread::sleep(WAIT_ROW_POLL);
    }
}

pub(crate) fn handle_session_report(
    home: &AgentsHome,
    emitter: &EventEmitter,
    req: &Request,
) -> Response {
    let Some(p) = req.params.as_object() else {
        return Response::err(req.id, ErrorCode::InvalidParams, "params must be an object");
    };
    let Some(harness) = str_field(p, "harness") else {
        return Response::err(req.id, ErrorCode::InvalidParams, "missing `harness`");
    };
    let session_id = payload_field(p, "session_id")
        .map(String::from)
        .or_else(|| str_field(p, "session_id"));
    let Some(session_id) = session_id else {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "no session id in the payload or --session-id",
        );
    };
    let transcript_path = payload_field(p, "transcript_path").map(String::from);
    let source = payload_field(p, "source")
        .map(String::from)
        .or_else(|| str_field(p, "source"));
    let agent_self = str_field(p, "agent_self");
    let wait_row = p
        .get("wait_row")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        .then_some(WAIT_ROW_CAP)
        .unwrap_or(Duration::ZERO);
    if !wait_row.is_zero() {
        wait_for_row(
            &home.registry_json(),
            &harness,
            &session_id,
            agent_self.as_deref(),
            wait_row,
        );
    }

    let mut outcome = Outcome::Unknown;
    let mut row_name: Option<String> = None;
    // Set when this report is the heir's first self-identification: the
    // primary id filled on a crowned row. The manifest arm runs after the
    // registry write commits.
    let mut manifest_arm: Option<(String, String, Option<String>)> = None;
    if let Err(e) = state::update_registry(&home.registry_json(), |r| {
        let Some(find) = find_row(&r.entries, &harness, &session_id, agent_self.as_deref()) else {
            outcome = Outcome::Unknown;
            return;
        };
        let idx = match find {
            Find::Holds(i) => i,
            Find::Backfill(i) => {
                // The same write the report store path makes: the canonical
                // id plus the in-memory alias, named by the ROW's harness.
                if r.entries[i].harness_name() == "codex" {
                    r.entries[i].harness_session_id = Some(session_id.clone());
                    r.entries[i].codex_session_id = Some(session_id.clone());
                } else {
                    r.entries[i].claude_session_uuid = Some(session_id.clone());
                }
                crowned_first_fill(&r.entries[i], &mut manifest_arm);
                i
            }
            Find::Named(i) => {
                let entry = &r.entries[i];
                let primary = entry.harness_session_id.as_deref().unwrap_or("");
                let related = entry.related_session_id.as_deref().unwrap_or("");
                if primary.is_empty() {
                    r.entries[i].harness_session_id = Some(session_id.clone());
                    crowned_first_fill(&r.entries[i], &mut manifest_arm);
                } else if related == session_id.as_str() {
                    // already held additively; the field stamps below still land
                } else if related.is_empty() {
                    r.entries[i].related_session_id = Some(session_id.clone());
                    outcome = Outcome::Stored {
                        related_filled: true,
                    };
                } else {
                    // A third distinct id: nothing written, both ids named -
                    // the same cap record_session_observation enforces.
                    outcome = Outcome::IdCap {
                        primary: primary.to_string(),
                        related: related.to_string(),
                    };
                    return;
                }
                i
            }
        };
        // The Stored default only stands for the fills that did not set it.
        if !matches!(outcome, Outcome::Stored { .. }) {
            outcome = Outcome::Stored {
                related_filled: false,
            };
        }
        let entry = &mut r.entries[idx];
        if entry.status == AgentStatus::Spawning {
            entry.status = AgentStatus::Live;
        }
        if let Some(tp) = &transcript_path {
            entry.transcript_path = Some(tp.clone());
        }
        if let Some(src) = &source {
            entry.start_source = Some(src.clone());
        }
        row_name = Some(entry.name.clone());
    }) {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("registry write failed during session report: {e}"),
        );
    }

    // The heir's first report is the manifest arm point (Python's
    // `_arm_crown_after_identification`): spawn-time succession has no heir
    // id at settle, so the transfer leaves the manifest naming the abdicating
    // session and the heir holds with no levers until this rewrite names it.
    if let Some((scope, cwd, row_harness)) = manifest_arm {
        arm_crown_manifest(
            &scope,
            &cwd,
            row_harness.as_deref(),
            &session_id,
            row_name.as_deref(),
            emitter,
        );
    }

    match outcome {
        Outcome::Stored { related_filled } => {
            let _ = emitter.emit(
                "session_report_stored",
                &json!({
                    "name": row_name,
                    "harness": harness,
                    "session_id": session_id,
                    "related_filled": related_filled,
                    "source": source,
                }),
            );
            Response::ok(
                req.id,
                json!({"stored": true, "related_filled": related_filled}),
            )
        }
        Outcome::Unknown => {
            let _ = emitter.emit(
                "session_report_dropped",
                &json!({"harness": harness, "session_id": session_id, "reason": "unknown_session"}),
            );
            Response::ok(
                req.id,
                json!({"stored": false, "dropped": "unknown_session"}),
            )
        }
        Outcome::IdCap { primary, related } => {
            let _ = emitter.emit(
                "session_report_dropped",
                &json!({
                    "harness": harness,
                    "session_id": session_id,
                    "reason": "id_cap",
                    "recorded": [primary, related],
                }),
            );
            Response::ok(
                req.id,
                json!({"stored": false, "dropped": "id_cap", "recorded": [primary, related]}),
            )
        }
    }
}

/// The crowned-row read that decides the arm: a crown stamp plus a scope on
/// the row the report just identified, with the row's own harness so the
/// rebind never leaves the manifest naming the arming path's harness.
fn crowned_first_fill(entry: &RegistryEntry, out: &mut Option<(String, String, Option<String>)>) {
    if entry.crown_level.is_none() {
        return;
    }
    if let Some(scope) = entry.crown_scope.as_deref().filter(|s| !s.is_empty()) {
        let harness = entry.harness.clone().filter(|h| !h.trim().is_empty());
        *out = Some((scope.to_string(), entry.cwd.clone(), harness));
    }
}

/// Rebind one scope's lead manifest to the session that just identified
/// itself, fail-soft like the Python arm: a failed write emits
/// `crown_manifest_arm_failed` and never fails the report. A scope with no
/// manifest yet is a fresh grant, not a stale succession; that arm stays
/// Python's. `row_harness` rides when the row carries one, so a manifest
/// naming the predecessor's harness never outlives the succession.
fn arm_crown_manifest(
    scope: &str,
    cwd: &str,
    row_harness: Option<&str>,
    session_id: &str,
    row_name: Option<&str>,
    emitter: &EventEmitter,
) {
    let fail = |error: String| {
        let _ = emitter.emit(
            "crown_manifest_arm_failed",
            &json!({
                "name": row_name,
                "scope": scope,
                "session_id": session_id,
                "error": error,
            }),
        );
    };
    // The stop hook matches the manifest id against the transcript basename,
    // so only a full uuid may name itself (the same shape Python's arm
    // refuses).
    if !crate::pane_keeper::is_full_uuid(session_id) {
        return;
    }
    let Some(space) = crate::paths::space_dir_opt(Path::new(cwd)) else {
        return;
    };
    let path = match crate::lead_state::manifest_path(&space, scope) {
        Ok(path) => path,
        Err(e) => return fail(e),
    };
    if !path.is_file() {
        return;
    }
    let mut fields = vec![("harness_session_id", session_id)];
    if let Some(row_harness) = row_harness {
        fields.push(("harness", row_harness));
    }
    if let Err(e) = crate::lead_state::set_manifest_fields(&space, scope, &fields, None) {
        fail(e);
    }
}

// ---------------------------------------------------------------------------
// client side: fno-agents session-report

/// `--harness` (required), `--agent-self`, `--session-id`, `--wait-row`.
fn build_session_report_params(rest: &[String]) -> Result<Value, String> {
    let mut params = Map::new();
    let mut it = crate::client_verbs::expand_eq(rest).into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--harness" => {
                params.insert(
                    "harness".into(),
                    Value::String(it.next().ok_or("--harness needs a value")?),
                );
            }
            "--agent-self" => {
                params.insert(
                    "agent_self".into(),
                    Value::String(it.next().ok_or("--agent-self needs a value")?),
                );
            }
            "--session-id" => {
                params.insert(
                    "session_id".into(),
                    Value::String(it.next().ok_or("--session-id needs a value")?),
                );
            }
            "--wait-row" => {
                params.insert("wait_row".into(), Value::Bool(true));
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if params.get("harness").and_then(Value::as_str).is_none() {
        return Err("session-report needs --harness".into());
    }
    Ok(Value::Object(params))
}

/// The raw hook payload rides on stdin verbatim; the daemon parses it. A
/// terminal (or unreadable) stdin means no payload - flags carry the report.
fn read_stdin_payload() -> Option<Value> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buf = Vec::new();
    let read = std::io::stdin()
        .lock()
        .by_ref()
        .take(MAX_PAYLOAD_BYTES)
        .read_to_end(&mut buf);
    read.ok()?;
    serde_json::from_slice::<Value>(&buf)
        .ok()
        .filter(Value::is_object)
}

fn read_spool(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn write_spool(path: &Path, lines: &[String]) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let tmp = parent.join(format!(".spool.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Exclusive lock over one spool read-modify-write. Concurrent reporters
/// (a fleet starting while the daemon is down, exactly when the spool is
/// needed) would otherwise each rename a private replacement over the
/// others' frames. Same sidecar-flock pattern as the registry.
fn spool_lock(path: &Path) -> Option<std::fs::File> {
    let lock_path = path.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .ok()?;
    file.lock().ok().map(|_| file)
}

/// Best-effort: a spool failure loses the frame rather than blocking the hook.
fn spool_request(home: &AgentsHome, req: &Request) {
    let line = json!({"method": req.method, "params": req.params}).to_string();
    let path = home.agent_hooks_spool();
    let _guard = spool_lock(&path);
    let mut lines = read_spool(&path);
    lines.push(line);
    if lines.len() > SPOOL_CAP_FRAMES {
        lines.drain(..lines.len() - SPOOL_CAP_FRAMES);
    }
    write_spool(&path, &lines);
}

enum Drain {
    /// Sent, or never going to send: drop the frame.
    Drop,
    /// Daemon down: keep the frame and stop draining.
    Keep,
}

async fn drain_attempt(home: &AgentsHome, line: &str) -> Drain {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Drain::Drop; // a frame we cannot parse can never send
    };
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return Drain::Drop;
    };
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    let req = Request::new(1, method.to_string(), params);
    match tokio::time::timeout(DRAIN_TIMEOUT, crate::client::call_if_running(home, &req)).await {
        Ok(Ok(_)) => Drain::Drop,
        // Down mid-drain: stop; the rest of the spool stays for next time.
        Ok(Err(crate::client::ClientError::DaemonNotRunning))
        | Ok(Err(crate::client::ClientError::DaemonUnresponsive { .. }))
        | Err(_) => Drain::Keep,
        // A real refusal (bad params) is not coming back: drop.
        Ok(Err(_)) => Drain::Drop,
    }
}

/// Pop the oldest frame under the lock; the caller sends it off-lock. A kill
/// between the pop and the send loses that one frame - the same bounded
/// window the rewrite-after-send shape had.
fn pop_spool(path: &Path) -> Option<String> {
    let _guard = spool_lock(path)?;
    let mut lines = read_spool(path);
    if lines.is_empty() {
        return None;
    }
    let line = lines.remove(0);
    write_spool(path, &lines);
    Some(line)
}

fn unpop_spool(path: &Path, line: &str) {
    if let Some(_guard) = spool_lock(path) {
        let mut lines = read_spool(path);
        lines.insert(0, line.to_string());
        write_spool(path, &lines);
    }
}

/// Replay the spool, FIFO. Called BEFORE the caller's own report is sent: a
/// spooled startup frame must land before a newer resume report, or the
/// replay would regress the row's transcript/source stamps to stale values.
async fn drain_spool(home: &AgentsHome) {
    let path = home.agent_hooks_spool();
    loop {
        let Some(line) = pop_spool(&path) else { return };
        if let Drain::Keep = drain_attempt(home, &line).await {
            unpop_spool(&path, &line);
            return;
        }
    }
}

/// The `report` verb's dispatcher. `--kind session` selects the SessionStart
/// transport; every other invocation is the inside-leg report, unchanged. The
/// SessionStart form rides the EXISTING action because the client action list
/// is shrink-only: no new verb, only an argument on `report`.
pub async fn run_report_dispatch(rest: &[String], home: &AgentsHome) -> i32 {
    let args = crate::client_verbs::expand_eq(rest);
    let mut is_session = false;
    let mut stripped: Vec<String> = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--kind" {
            match it.next() {
                Some(v) if v == "session" => {
                    is_session = true;
                    continue;
                }
                other => {
                    stripped.push(a);
                    if let Some(v) = other {
                        stripped.push(v);
                    }
                    continue;
                }
            }
        }
        stripped.push(a);
    }
    if !is_session {
        return crate::client_verbs::run_report(rest, home).await;
    }
    run_session_report(&stripped, home).await
}

/// `fno-agents report --kind session --harness <name> [--agent-self <row>]
/// [--session-id <id>] [--wait-row]` - the thin SessionStart transport. The
/// raw hook payload rides on stdin. Never lazy-starts a daemon; spools to a
/// capped file when it is down. Always exits 0 on a delivered-or-spooled
/// report, so a session start never blocks and never reds.
pub async fn run_session_report(rest: &[String], home: &AgentsHome) -> i32 {
    let params = match build_session_report_params(rest) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };
    let mut params = params;
    if let Some(payload) = read_stdin_payload() {
        params["payload"] = payload;
    }
    let req = Request::new(1, "agent.session_report", params);
    // FIFO: spooled frames go first, so a replayed startup report can never
    // regress the row behind the newer report this invocation carries. The
    // drain no-ops on an empty spool and keeps its frames when the daemon is
    // down; the send below then spools alongside them.
    drain_spool(home).await;
    let sent = match tokio::time::timeout(SEND_TIMEOUT, crate::client::call_if_running(home, &req))
        .await
    {
        Err(_) => false,
        Ok(Ok(_)) => true,
        Ok(Err(crate::client::ClientError::DaemonNotRunning)) => false,
        Ok(Err(crate::client::ClientError::DaemonUnresponsive { .. })) => false,
        Ok(Err(e)) => {
            eprintln!("fno-agents: session-report failed: {e}");
            return 1;
        }
    };
    if !sent {
        spool_request(home, &req);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AgentsHome;

    fn temp_home(tag: &str) -> (tempfile::TempDir, AgentsHome) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = AgentsHome::at(dir.path().join(tag));
        std::fs::create_dir_all(home.root()).expect("home root");
        (dir, home)
    }

    fn emitter(home: &AgentsHome) -> EventEmitter {
        EventEmitter::new(home.events_jsonl(), "test")
    }

    fn req(params: Value) -> Request {
        Request::new(1, "agent.session_report", params)
    }

    fn report_params(harness: &str, sid: &str, extra: Value) -> Value {
        let mut p = json!({"harness": harness, "session_id": sid});
        if let (Some(base), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        p
    }

    fn seed_registry(home: &AgentsHome, entry: Value) {
        let path = home.registry_json();
        let reg = json!({"schema_version": 1, "agents": [entry]});
        std::fs::write(path, serde_json::to_vec_pretty(&reg).unwrap()).expect("seed");
    }

    fn read_rows(home: &AgentsHome) -> Vec<RegistryEntry> {
        crate::daemon::load_registry_asserted(&home.registry_json())
            .expect("registry readable")
            .entries
    }

    fn response_json(resp: &Response) -> Value {
        serde_json::to_value(resp).expect("response json")
    }

    fn claude_row() -> Value {
        json!({
            "name": "w1",
            "cwd": "/repo",
            "status": "live",
            "created_at": "2026-09-26T00:00:00Z",
            "harness": "claude",
            "short_id": "3228ccad"
        })
    }

    #[test]
    fn missing_harness_is_invalid_params() {
        let (_d, home) = temp_home("no-harness");
        let resp = handle_session_report(&home, &emitter(&home), &req(json!({})));
        assert_eq!(response_json(&resp)["error"]["code"], "invalid_params");
    }

    #[test]
    fn payload_session_id_wins_and_stamps_transcript_and_source() {
        let (_d, home) = temp_home("stamp");
        seed_registry(&home, claude_row());
        let params = json!({
            "harness": "claude",
            "session_id": "env-fallback",
            "payload": {
                "session_id": "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d",
                "transcript_path": "/t/w1.jsonl",
                "source": "startup"
            }
        });
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        let row = &read_rows(&home)[0];
        assert_eq!(
            row.harness_session_id.as_deref(),
            Some("3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d")
        );
        assert_eq!(row.transcript_path.as_deref(), Some("/t/w1.jsonl"));
        assert_eq!(row.start_source.as_deref(), Some("startup"));
    }

    #[test]
    fn backfill_prefix_fills_the_waiting_bg_row() {
        let (_d, home) = temp_home("backfill");
        seed_registry(&home, claude_row());
        let params = report_params("claude", "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d", json!({}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        let row = &read_rows(&home)[0];
        assert_eq!(
            row.claude_session_uuid.as_deref(),
            Some("3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d")
        );
    }

    #[test]
    fn named_row_takes_its_first_id_and_promotes_spawning() {
        let (_d, home) = temp_home("named-first");
        seed_registry(
            &home,
            json!({
                "name": "w1", "cwd": "/repo", "status": "spawning",
                "created_at": "2026-09-26T00:00:00Z", "harness": "codex"
            }),
        );
        let params = report_params(
            "codex",
            "0197abcd-1234-7abc-9def-0123456789ab",
            json!({"agent_self": "w1"}),
        );
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        let row = &read_rows(&home)[0];
        assert_eq!(row.status, AgentStatus::Live);
        assert_eq!(
            row.harness_session_id.as_deref(),
            Some("0197abcd-1234-7abc-9def-0123456789ab")
        );
    }

    #[test]
    fn resumed_id_fills_the_related_slot_not_the_primary() {
        let (_d, home) = temp_home("related");
        seed_registry(
            &home,
            json!({
                "name": "w1", "cwd": "/repo", "status": "live",
                "created_at": "2026-09-26T00:00:00Z", "harness": "codex",
                "harness_session_id": "0197aaaa-1234-7abc-9def-0123456789ab"
            }),
        );
        let params = report_params(
            "codex",
            "0197bbbb-1234-7abc-9def-0123456789ab",
            json!({"agent_self": "w1", "payload": {"source": "resume"}}),
        );
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["related_filled"], true);
        let row = &read_rows(&home)[0];
        assert_eq!(
            row.harness_session_id.as_deref(),
            Some("0197aaaa-1234-7abc-9def-0123456789ab")
        );
        assert_eq!(
            row.related_session_id.as_deref(),
            Some("0197bbbb-1234-7abc-9def-0123456789ab")
        );
        assert_eq!(row.start_source.as_deref(), Some("resume"));
    }

    #[test]
    fn third_distinct_id_refuses_and_writes_nothing() {
        let (_d, home) = temp_home("cap");
        seed_registry(
            &home,
            json!({
                "name": "w1", "cwd": "/repo", "status": "live",
                "created_at": "2026-09-26T00:00:00Z", "harness": "codex",
                "harness_session_id": "0197aaaa-1234-7abc-9def-0123456789ab",
                "related_session_id": "0197bbbb-1234-7abc-9def-0123456789ab"
            }),
        );
        let params = report_params(
            "codex",
            "0197cccc-1234-7abc-9def-0123456789ab",
            json!({"agent_self": "w1"}),
        );
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        let body = response_json(&resp);
        assert_eq!(body["result"]["stored"], false);
        assert_eq!(body["result"]["dropped"], "id_cap");
        // The refusal writes nothing: the related slot holds the SECOND id,
        // the third never lands, and no transcript stamp sneaks in.
        let row = &read_rows(&home)[0];
        assert_eq!(
            row.related_session_id.as_deref(),
            Some("0197bbbb-1234-7abc-9def-0123456789ab")
        );
        assert_eq!(row.transcript_path, None);
    }

    #[test]
    fn unknown_row_drops_without_creating_one() {
        let (_d, home) = temp_home("unknown");
        let params = report_params("codex", "0197dddd-1234-7abc-9def-0123456789ab", json!({}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        let body = response_json(&resp);
        assert_eq!(body["result"]["stored"], false);
        assert_eq!(body["result"]["dropped"], "unknown_session");
        assert!(read_rows(&home).is_empty());
    }

    /// A `spawn --crown --succeed` transfer leaves the manifest
    /// naming the abdicating session, and the heir's levers (shape, term)
    /// read that manifest, so the heir holds with no levers. The heir's
    /// first session report is the arm point: the fill rebinds the manifest
    /// to the heir, including the harness when the row carries one (a claude
    /// predecessor's `harness: claude` must not outlive a codex heir). A
    /// later resume (related-slot fill) rewrites nothing.
    #[test]
    fn a_crowned_heirs_first_report_binds_the_manifest_to_its_session() {
        let lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let home_root = dir.path().join("home");
        std::fs::create_dir_all(&home_root).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &home_root);
        let home = AgentsHome::at(home_root);
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let heir = "01a10f07-d704-75a0-a460-7913efe8971b";
        let predecessor = "99473043-aaaa-4bbb-8ccc-ddddeeeeeeee";
        seed_registry(
            &home,
            json!({
                "name": "lead-wren", "cwd": cwd.display().to_string(),
                "status": "spawning", "created_at": "2026-10-05T20:06:00Z",
                "harness": "codex", "crown_level": 2,
                "crown_scope": "x-aaaa", "crown_grantor": "vellum"
            }),
        );
        let kings = crate::paths::space_dir(&cwd).join("kings");
        std::fs::create_dir_all(&kings).unwrap();
        let manifest = kings.join("x-aaaa.md");
        std::fs::write(
            &manifest,
            format!(
                "---\nterm: span:96h\nscope: x-aaaa\nshape: court\nharness: claude\n\
                 harness_session_id: {predecessor}\ncrown_level: 2\ncrown_scope: x-aaaa\n---\n"
            ),
        )
        .unwrap();

        let params = report_params("codex", heir, json!({"agent_self": "lead-wren"}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        let bound = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            bound.contains(&format!("harness_session_id: {heir}")),
            "{bound}"
        );
        // The harness rides with the id: the row's own harness wins, and the
        // predecessor's `harness: claude` cannot outlive the succession.
        assert!(bound.contains("harness: codex"), "{bound}");
        assert!(!bound.contains("harness: claude"), "{bound}");
        // Everything but the holder identity survives the rewrite.
        assert!(bound.contains("term: span:96h"), "{bound}");
        assert!(bound.contains("shape: court"), "{bound}");
        assert!(bound.contains("crown_grantor: vellum"), "{bound}");
        assert_eq!(read_rows(&home)[0].status, AgentStatus::Live);

        // A resume fills the related slot and never rewrites the manifest.
        let resume = "0197bbbb-1234-7abc-9def-0123456789ab";
        let params = report_params("codex", resume, json!({"agent_self": "lead-wren"}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["related_filled"], true);
        let bound = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            bound.contains(&format!("harness_session_id: {heir}")),
            "{bound}"
        );
        drop(lock);
    }

    #[tokio::test]
    async fn spool_round_trips_through_cap_and_drain() {
        let (_d, home) = temp_home("spool");
        let first = req(report_params("codex", "sid-a", json!({})));
        spool_request(&home, &first);
        spool_request(&home, &req(report_params("codex", "sid-b", json!({}))));
        for i in 0..(SPOOL_CAP_FRAMES + 10) {
            spool_request(
                &home,
                &req(report_params("codex", &format!("sid-{i}"), json!({}))),
            );
        }
        let path = home.agent_hooks_spool();
        let lines = read_spool(&path);
        assert_eq!(lines.len(), SPOOL_CAP_FRAMES, "cap holds");
        // The two oldest frames (sid-a, sid-b) were dropped, newest kept.
        assert!(!lines[0].contains("sid-a"));
        assert!(lines.last().expect("nonempty").contains("sid-137"));
        // A drained spool is empty; a keep mid-drain leaves the tail.
        write_spool(&path, &lines);
        drain_spool(&home).await;
        // The daemon is down here, so every frame is kept - the file survives.
        assert_eq!(read_spool(&path).len(), SPOOL_CAP_FRAMES);
    }

    #[test]
    fn params_builder_requires_harness_and_rejects_unknown() {
        let err = build_session_report_params(&["--session-id".into(), "x".into()]);
        assert!(err.unwrap_err().contains("--harness"));
        let err =
            build_session_report_params(&["--harness".into(), "codex".into(), "--nope".into()]);
        assert!(err.unwrap_err().contains("unknown flag"));
        let ok = build_session_report_params(&[
            "--harness=claude".into(),
            "--agent-self".into(),
            "w1".into(),
            "--wait-row".into(),
        ])
        .unwrap();
        assert_eq!(ok["harness"], "claude");
        assert_eq!(ok["agent_self"], "w1");
        assert_eq!(ok["wait_row"], true);
    }
}

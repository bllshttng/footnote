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

/// A params member that is itself an object (the claimed launch record rides
/// as one; a string member is not).
fn params_object<'a>(p: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    p.get(key).filter(|v| v.is_object())
}

/// Which row a report lands on, and what the match means for the id.
pub(crate) enum Find {
    /// `entry_holds_session` already answers true: the row records this id.
    Holds(usize),
    /// The claude short-id backfill match: a bg row awaiting its full uuid.
    Backfill(usize),
    /// The named row (`FNO_AGENT_SELF`): the durable key a resumed worker
    /// corrects on.
    Named(usize),
}

pub(crate) fn find_row(
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
    Stored {
        related_filled: bool,
    },
    /// No row held the id and the handler minted one: a launch-record child
    /// (`native: true`) joins as the launcher's row; a session with no record
    /// is the user's own (decision D3 revised, recorded on the node): origin
    /// operator,
    /// an occupant toward the cap, never refused, managed or reaped.
    Minted {
        native: bool,
    },
    Unknown,
    IdCap {
        primary: String,
        related: String,
    },
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
    let report_cwd = payload_field(p, "cwd")
        .map(String::from)
        .or_else(|| str_field(p, "cwd"));
    // A claimed launch record rides the request (run_session_report attaches
    // it): the child that reported is the launcher's launch.
    let launch_record: Option<crate::launch_record::Record> =
        params_object(p, "launch_record").and_then(|v| serde_json::from_value(v.clone()).ok());
    let launch_child_name = str_field(p, "launch_child_name");
    // A proven pane claim rides as {session, pane_id}; anything else (absent,
    // malformed) is ignored - the report still stores normally, it just
    // binds no pane.
    let pane_claim: Option<crate::pane_claim::PaneClaim> = params_object(p, "pane").and_then(|v| {
        let obj = v.as_object()?;
        let session = obj.get("session")?.as_str()?.to_string();
        let pane_id = obj.get("pane_id")?.as_u64()?;
        Some(crate::pane_claim::PaneClaim { session, pane_id })
    });
    let mut pane_rebind: Option<(Option<crate::state::MuxRef>, crate::state::MuxRef)> = None;
    // A daemon-hosted harness reports from the per-machine daemon, so the
    // client-side ancestry proof can never fire for it. When no claim rode
    // the report and the harness is opencode, the pane-side proof names the
    // pane instead: keeper panes whose subtree runs the opencode TUI at the
    // report's working directory. It shells out and must not hold the
    // registry lock, so it runs here, before the locked write. Every
    // failure is fail-soft: the row still registers through the arms below.
    let pane_side = match (
        harness == "opencode",
        pane_claim.as_ref(),
        report_cwd.as_deref(),
    ) {
        (true, None, Some(cwd)) => pane_side_lookup(cwd),
        _ => None,
    };
    let effective_claim = pane_claim.clone().or(pane_side);
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

    // The mint prediction runs BEFORE the locked write: building the row does
    // transcript IO (title, model), which must not hold the registry lock.
    // The write closure re-decides absence under the lock.
    let pending_mint = crate::native_adopt::prepare_mint(
        home,
        &harness,
        &session_id,
        launch_record.as_ref(),
        launch_child_name.as_deref(),
        agent_self.as_deref(),
        transcript_path.as_deref(),
        source.as_deref(),
        report_cwd.as_deref(),
    );

    let mut outcome = Outcome::Unknown;
    let mut row_name: Option<String> = None;
    // The freshness stamp every report leaves on the row: the capacity
    // reader's liveness source for rows no probe reaches (d-d0c63de7).
    let now_stamp = crate::daemon::now_rfc3339_like();
    // Set when this report is the successor's first self-identification: the
    // primary id filled on a promoted row. The manifest arm runs after the
    // registry write commits.
    let mut manifest_arm: Option<(String, String, Option<String>)> = None;
    if let Err(e) = state::update_registry(&home.registry_json(), |r| {
        let Some(find) = find_row(&r.entries, &harness, &session_id, agent_self.as_deref()) else {
            match &pending_mint {
                Some(mint) => {
                    let mut entry = mint.entry.clone();
                    entry.name = crate::native_adopt::unique_name(&r.entries, &entry.name);
                    // The report that mints the row is its first report.
                    entry.last_report_at = Some(now_stamp.clone());
                    row_name = Some(entry.name.clone());
                    r.entries.push(entry);
                    let new_idx = r.entries.len() - 1;
                    if let Some(claim) = &effective_claim {
                        pane_rebind = crate::pane_claim::bind(&mut r.entries, new_idx, claim);
                    }
                    outcome = Outcome::Minted {
                        native: mint.native,
                    };
                }
                None => {
                    outcome = Outcome::Unknown;
                }
            }
            return;
        };
        let idx = match find {
            Find::Holds(i) => {
                // An operator row's own re-identification (`claude
                // --resume` typed into a pane); a worker's mux is the
                // keeper's business, not this report's.
                if r.entries[i].origin.as_deref() == Some("operator") {
                    if let Some(claim) = &effective_claim {
                        pane_rebind = crate::pane_claim::bind(&mut r.entries, i, claim);
                    }
                }
                i
            }
            Find::Backfill(i) => {
                // The same write the report store path makes: the canonical
                // id plus the in-memory alias, named by the ROW's harness.
                if r.entries[i].harness_name() == "codex" {
                    r.entries[i].harness_session_id = Some(session_id.clone());
                    r.entries[i].codex_session_id = Some(session_id.clone());
                } else {
                    r.entries[i].claude_session_uuid = Some(session_id.clone());
                }
                promoted_first_fill(&r.entries[i], &mut manifest_arm);
                // `wait_row` is the pane-worker marker (parity with the
                // retired Python heal's `FNO_AGENT_ROW_PENDING` gate): only a
                // pane-substrate worker's row may take a pane bind here.
                if !wait_row.is_zero() {
                    if let Some(claim) = &effective_claim {
                        pane_rebind = crate::pane_claim::bind(&mut r.entries, i, claim);
                    }
                }
                i
            }
            Find::Named(i) => {
                let entry = &r.entries[i];
                let primary = entry.harness_session_id.as_deref().unwrap_or("");
                let related = entry.related_session_id.as_deref().unwrap_or("");
                if primary.is_empty() {
                    r.entries[i].harness_session_id = Some(session_id.clone());
                    promoted_first_fill(&r.entries[i], &mut manifest_arm);
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
                // Same pane-worker gate as the backfill arm above.
                if !wait_row.is_zero() {
                    if let Some(claim) = &effective_claim {
                        pane_rebind = crate::pane_claim::bind(&mut r.entries, i, claim);
                    }
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
        entry.last_report_at = Some(now_stamp.clone());
        row_name = Some(entry.name.clone());
    }) {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("registry write failed during session report: {e}"),
        );
    }

    // The successor's first report is the manifest arm point (Python's
    // `_arm_role_after_identification`): spawn-time succession has no successor
    // id at settle, so the transfer leaves the manifest naming the stepping_down
    // session and the successor holds with no levers until this rewrite names it.
    if let Some((scope, cwd, row_harness)) = manifest_arm {
        arm_role_manifest(
            &scope,
            &cwd,
            row_harness.as_deref(),
            &session_id,
            row_name.as_deref(),
            emitter,
        );
    }

    // The row moved to a new pane: announce it, same kind and nested
    // old/new shape the retired Python heal already emitted
    // (`_heal_own_mux_ref`); the schema's legacy entry for this kind needs
    // no change. A repeat report on the same pane leaves this unset.
    if let Some((old, new)) = pane_rebind {
        let _ = emitter.emit(
            "session_pane_rebound",
            &json!({
                "old": old,
                "new": new,
                "name": row_name,
                "harness": harness,
            }),
        );
    }

    match outcome {
        Outcome::Minted { native } => {
            // A native launch's birth names its parent through the record and
            // carries `launch: "native"` (decision D7). Every key a spawned
            // birth carries is present (pid unknown at adopt rides null); the
            // superset test pins the rule.
            if native {
                if let Some(record) = &launch_record {
                    let lineage = crate::state::Lineage::captured((
                        Some(record.launcher_session.clone()),
                        record.launcher_harness.clone(),
                        record.launcher_cwd.clone(),
                    ));
                    let extras = json!({
                        "spawn_id": crate::launch_record::file_key(record),
                        "harness_session_id": session_id.clone(),
                        "cwd": record.launch.cwd.clone(),
                        "substrate": crate::launch_record::native_substrate_word(&harness),
                        "pid": Value::Null,
                        "launch": "native",
                    });
                    let name = row_name.clone().unwrap_or_default();
                    let _ = emitter.emit(
                        "agent_spawned",
                        &crate::spawn_edge::birth_event(&name, &lineage, extras),
                    );
                }
            }
            let _ = emitter.emit(
                "session_report_stored",
                &json!({
                    "name": row_name,
                    "harness": harness,
                    "session_id": session_id,
                    "related_filled": false,
                    "minted": if native { "native" } else { "operator" },
                    "source": source,
                }),
            );
            Response::ok(
                req.id,
                json!({"stored": true, "minted": if native { "native" } else { "operator" }}),
            )
        }
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

/// The pane-side lookup the opencode arm calls: reads keeper pane truth
/// (`fno mux pane ls --json`, `fno mux pane keeper list --json`, joined on
/// child_pid; the keeper row supplies the mux session the pane row lacks)
/// and census process truth, then hands both to the pure
/// `pane_claim::from_pane_side`. Fail-soft by contract: a missing fno
/// binary, a downed mux, or unparseable output returns None, and the row
/// still registers through the report arms unbound.
fn pane_side_lookup(report_cwd: &str) -> Option<crate::pane_claim::PaneClaim> {
    let fno = crate::loopcheck::loopcheck_fno_bin();
    let panes_out = std::process::Command::new(&fno)
        .args(["mux", "pane", "ls", "--json"])
        .output()
        .ok()?;
    if !panes_out.status.success() {
        return None;
    }
    let keepers_out = std::process::Command::new(&fno)
        .args(["mux", "pane", "keeper", "list", "--json"])
        .output()
        .ok()?;
    if !keepers_out.status.success() {
        return None;
    }
    let panes: Vec<Value> = serde_json::from_slice(&panes_out.stdout).ok()?;
    let keepers: Vec<Value> = serde_json::from_slice(&keepers_out.stdout).ok()?;
    let mut pane_rows: Vec<crate::pane_claim::PaneRow> = Vec::new();
    for keeper in &keepers {
        let Some(session) = keeper.get("session").and_then(Value::as_str) else {
            continue;
        };
        let Some(child) = keeper.get("child_pid").and_then(Value::as_u64) else {
            continue;
        };
        if keeper.get("stale").is_some() {
            continue;
        }
        let Some(pane_row) = panes
            .iter()
            .find(|p| p.get("child_pid").and_then(Value::as_u64) == Some(child))
        else {
            continue;
        };
        pane_rows.push(crate::pane_claim::PaneRow {
            session: session.to_string(),
            pane_id: pane_row.get("pane_id").and_then(Value::as_u64).unwrap_or(0),
            child_pid: child as u32,
            cwd: pane_row
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            alt_cwd: pane_row
                .get("pane_cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    let table = crate::census::process_table();
    crate::pane_claim::from_pane_side(&pane_rows, &table.0, report_cwd)
}

/// The promoted-row read that decides the arm: a role stamp plus a scope on
/// the row the report just identified, with the row's own harness so the
/// rebind never leaves the manifest naming the arming path's harness.
fn promoted_first_fill(entry: &RegistryEntry, out: &mut Option<(String, String, Option<String>)>) {
    if entry.role_level.is_none() {
        return;
    }
    if let Some(scope) = entry.role_scope.as_deref().filter(|s| !s.is_empty()) {
        let harness = entry.harness.clone().filter(|h| !h.trim().is_empty());
        *out = Some((scope.to_string(), entry.cwd.clone(), harness));
    }
}

/// Rebind one scope's lead manifest to the session that just identified
/// itself, fail-soft like the Python arm: a failed write emits
/// `role_manifest_arm_failed` and never fails the report. A scope with no
/// manifest yet is a fresh grant, not a stale succession; that arm stays
/// Python's. `row_harness` rides when the row carries one, so a manifest
/// naming the predecessor's harness never outlives the succession.
fn arm_role_manifest(
    scope: &str,
    cwd: &str,
    row_harness: Option<&str>,
    session_id: &str,
    row_name: Option<&str>,
    emitter: &EventEmitter,
) {
    let fail = |error: String| {
        let _ = emitter.emit(
            "role_manifest_arm_failed",
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
            "--origin-only" => {
                params.insert("origin_only".into(), Value::Bool(true));
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

/// Write the origin record beside this session's transcript (see
/// `crate::session_origin`). Best effort: an error never fails the hook. A
/// source of `resume` writes nothing: the record names the machine a session
/// BEGAN on, and a resume is not that machine's claim to make.
fn record_origin(params: &Value) {
    let payload = params.get("payload");
    let source = payload
        .and_then(|p| p.get("source"))
        .or_else(|| params.get("source"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if source == "resume" {
        return;
    }
    let harness = params
        .get("harness")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let sid = payload
        .and_then(|p| p.get("session_id"))
        .or_else(|| params.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if sid.is_empty() {
        return;
    }
    let transcript = payload
        .and_then(|p| p.get("transcript_path"))
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .or_else(|| {
            if harness == "codex" {
                crate::codex_store::codex_rollout_path(None, sid)
            } else {
                None
            }
        });
    let Some(transcript) = transcript else {
        return;
    };
    let _ = crate::session_origin::write_if_absent(
        &crate::session_origin::SessionOrigin::for_this_machine(harness, sid, &transcript),
    );
}

/// Claim the launch record this session is the child of, and stamp its
/// node's first-launch edge (see `crate::launch_record`). Every harness sends
/// this report, so this is the one claim point. A resume is not a launch.
/// Returns the claimed record and the child's own name (the launch-record
/// claim already matched on it), so the report can carry both to the daemon
/// and the adopt overlays the record onto the minted row.
fn claim_launch(
    params: &Value,
    home: &AgentsHome,
) -> (Option<crate::launch_record::Record>, Option<String>) {
    let field = |key: &str| {
        params
            .get("payload")
            .and_then(|p| p.get(key))
            .or_else(|| params.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let session_id = field("session_id");
    if field("source") == "resume" || session_id.is_empty() {
        return (None, None);
    }
    let cwd = Some(field("cwd"))
        .filter(|c| !c.is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let child_name = crate::launch_record::child_name_from_env();
    let child = crate::launch_record::Child {
        harness: field("harness"),
        session_id,
        cwd,
        name: child_name.clone(),
        started_at_ms: crate::claims::now_ms(),
    };
    let record = crate::launch_record::claim_and_stamp(
        home.root(),
        &crate::backlog::settings::graph_path(),
        &child,
    );
    (record, child_name)
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
    let (record, child_name) = claim_launch(&params, home);
    record_origin(&params);
    if let Some(record) = &record {
        // A claimed record rides the request: the daemon adopts this session
        // as the launcher's child even when no row exists yet.
        params["launch_record"] = serde_json::to_value(record).unwrap_or(Value::Null);
        if let Some(name) = child_name {
            params["launch_child_name"] = Value::String(name);
        }
    }
    // A proven pane claim rides the request too: a session outside any
    // pane, or one the ancestry walk cannot prove, pays nothing and adds
    // nothing. Without one, the reporter's working directory rides instead,
    // so a daemon-hosted harness (opencode's plugin runs inside the
    // per-machine serve daemon) can still be bound from the pane side.
    if let Some(claim) = crate::pane_claim::ambient() {
        params["pane"] = json!({"session": claim.session, "pane_id": claim.pane_id});
    } else if let Ok(cwd) = std::env::current_dir() {
        params["cwd"] = Value::String(cwd.display().to_string());
    }
    // --origin-only writes the record and sends nothing - EXCEPT for a child
    // that claimed a launch record: the adopt must never lose the parent edge
    // to the origin-only path, so the report rides with the record attached.
    if params.get("origin_only").and_then(Value::as_bool) == Some(true) && record.is_none() {
        return 0;
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
        // The judge refuses an event whose type does not declare the emitter's
        // source, and `let _ =` at the emit sites swallows that refusal. The
        // report RPC runs inside the daemon, so the tests emit as "daemon" -
        // the one source both minted types declare.
        EventEmitter::new(home.events_jsonl(), "daemon")
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

    /// Backdate a file so the prune's 24-hour floor reads it as stale.
    fn backdate(path: &std::path::Path, age: std::time::Duration) {
        let past = std::time::SystemTime::now() - age;
        let secs = past
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let times = [
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: secs,
                tv_nsec: 0,
            },
        ];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid C string from an owned path; the call only sets times.
        unsafe {
            libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
        }
    }

    #[test]
    fn payload_session_id_wins_and_stamps_transcript_and_source() {
        let (_d, home) = temp_home("stamp");
        seed_registry(&home, claude_row());
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("w1.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        // An orphan this machine wrote over a day ago, its transcript gone:
        // the next write in this folder prunes exactly it (AC2-PRUNE).
        let orphan_sid = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let orphan = dir.path().join(format!("{orphan_sid}.fno.json"));
        std::fs::write(
            &orphan,
            serde_json::to_vec(&crate::session_origin::SessionOrigin {
                machine: crate::session_origin::this_machine(),
                host: "elsewhere".into(),
                harness: "claude".into(),
                session_id: orphan_sid.into(),
                transcript_path: "/gone/w1.jsonl".into(),
                recorded_at: "2026-09-01T00:00:00Z".into(),
            })
            .unwrap(),
        )
        .unwrap();
        backdate(&orphan, std::time::Duration::from_secs(2 * 24 * 3600));
        let path_str = transcript.to_string_lossy().to_string();
        let params = json!({
            "harness": "claude",
            "session_id": "env-fallback",
            "payload": {
                "session_id": "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d",
                "transcript_path": path_str,
                "source": "startup"
            }
        });
        let resp = handle_session_report(&home, &emitter(&home), &req(params.clone()));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        let row = &read_rows(&home)[0];
        assert_eq!(
            row.harness_session_id.as_deref(),
            Some("3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d")
        );
        assert_eq!(row.transcript_path.as_deref(), Some(path_str.as_str()));
        assert_eq!(row.start_source.as_deref(), Some("startup"));
        // The startup report wrote the origin record beside the transcript;
        // a second write is a no-op (first machine wins); the prune removed
        // only the stale orphan (AC1-HP, AC2-PRUNE).
        let record = dir
            .path()
            .join("3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d.fno.json");
        record_origin(&params);
        assert!(record.exists());
        assert!(!orphan.exists(), "stale orphan pruned");
        let first = std::fs::read(&record).unwrap();
        record_origin(&params);
        assert_eq!(std::fs::read(&record).unwrap(), first, "first machine wins");
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
    fn the_session_report_stamps_last_report_at() {
        let (_d, home) = temp_home("report-stamp");
        seed_registry(&home, claude_row());
        let params = report_params("claude", "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d", json!({}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        let row = &read_rows(&home)[0];
        let stamp = row
            .last_report_at
            .as_deref()
            .expect("the report stamps last_report_at");
        assert!(
            crate::state::rfc3339_like_to_secs(stamp).is_some(),
            "the stamp parses: {stamp}"
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
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("rollout.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        let params = report_params(
            "codex",
            "0197bbbb-1234-7abc-9def-0123456789ab",
            json!({"agent_self": "w1", "payload": {"source": "resume", "transcript_path": transcript.to_string_lossy()}}),
        );
        let resp = handle_session_report(&home, &emitter(&home), &req(params.clone()));
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
        // A resume writes no origin record: the record names the machine the
        // session BEGAN on, and the resuming machine is not it (AC2-ERR).
        record_origin(&params);
        assert!(!dir
            .path()
            .join("0197bbbb-1234-7abc-9def-0123456789ab.fno.json")
            .exists());
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

    /// Decision D3 revised: a session no row holds mints an operator row - the
    /// user's own session. Occupant toward the cap, never managed.
    #[test]
    fn an_unknown_session_mints_an_operator_row() {
        let (_d, home) = temp_home("unknown");
        let params = report_params("codex", "0197dddd-1234-7abc-9def-0123456789ab", json!({}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        let body = response_json(&resp);
        assert_eq!(body["result"]["stored"], true);
        assert_eq!(body["result"]["minted"], "operator");
        let rows = read_rows(&home);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin.as_deref(), Some("operator"));
        assert_eq!(rows[0].status, AgentStatus::Live);
        assert_eq!(rows[0].substrate, None);
        // No birth event: the user's session is not an agent birth. The mint
        // says so through session_report_stored's minted field instead.
        let births = crate::event_store::query_events(
            &home.events_jsonl(),
            &crate::event_store::EventQuery::of_types(&["agent_spawned"]),
        )
        .unwrap();
        assert!(births.is_empty(), "{births:?}");
        // A second report of the same id stamps the row it minted; it never
        // mints twice.
        let params = report_params("codex", "0197dddd-1234-7abc-9def-0123456789ab", json!({}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["stored"], true);
        assert!(response_json(&resp)["result"]["minted"].is_null());
        assert_eq!(read_rows(&home).len(), 1);
    }

    /// The native adopt: a report whose launch record rides the request mints
    /// the launcher's row - the launcher triple as the parent edge, the
    /// record's node on the row, the record's name winning the name
    /// precedence, and one `agent_spawned` birth carrying `launch: "native"`.
    #[test]
    fn a_launch_record_child_adopts_with_parent_node_and_birth() {
        let (_d, home) = temp_home("native");
        let record = crate::launch_record::Record {
            launch: crate::launch_record::Launch {
                harness: "claude".into(),
                name: Some("w1".into()),
                node: Some("x-aaaa".into()),
                slot: None,
                provider: None,
                cwd: "/repo".into(),
            },
            launcher_session: "lead-7".into(),
            launcher_harness: Some("claude".into()),
            launcher_cwd: Some("/repo".into()),
            launched_at_ms: 1_000,
            command: String::new(),
            child_session: Some("3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d".into()),
            claimed_at_ms: Some(2_000),
        };
        let params = json!({
            "harness": "claude",
            "session_id": "3228ccad-c078-4f2e-9a51-6d1f0a2b3c4d",
            "launch_record": serde_json::to_value(&record).unwrap(),
        });
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        let body = response_json(&resp);
        assert_eq!(body["result"]["stored"], true, "{body}");
        assert_eq!(body["result"]["minted"], "native");
        let rows = read_rows(&home);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin.as_deref(), Some("spawn"));
        assert_eq!(rows[0].name, "w1");
        assert_eq!(rows[0].node.as_deref(), Some("x-aaaa"));
        assert_eq!(rows[0].spawned_by_session.as_deref(), Some("lead-7"));
        assert_eq!(rows[0].substrate.as_deref(), Some("thread"));
        assert_eq!(rows[0].status, AgentStatus::Live);
        let births = crate::event_store::query_events(
            &home.events_jsonl(),
            &crate::event_store::EventQuery::of_types(&["agent_spawned"]),
        )
        .unwrap();
        assert_eq!(births.len(), 1, "{births:?}");
        assert!(births[0].line.contains("launch"), "{}", births[0].line);
    }

    /// Decision D8: a record's name that collides with a live row takes a
    /// numeric suffix, so the adopt never lands two live rows on one handle.
    /// AC9: the native birth carries every key the spawned birth carries.
    /// AC10: removing one key fails the helper, naming it.
    #[test]
    fn native_birth_keys_are_a_superset_of_the_spawned_ones() {
        let lineage = crate::state::Lineage::captured((
            Some("lead-1".into()),
            Some("claude".into()),
            Some("/repo".into()),
        ));
        let spawned = crate::spawn_edge::birth_event(
            "w-spawned",
            &lineage,
            json!({
                "spawn_id": "spawn-1",
                "pid": 4242,
                "harness_session_id": "sid-a",
                "cwd": "/repo",
                "substrate": "thread",
            }),
        );
        let mut native = crate::spawn_edge::birth_event(
            "w-native",
            &lineage,
            json!({
                "spawn_id": "launch-1000-ab12cd34.json",
                "pid": Value::Null,
                "harness_session_id": "sid-b",
                "cwd": "/repo",
                "substrate": "thread",
                "launch": "native",
            }),
        );
        crate::native_adopt::assert_birth_superset(&native, &spawned);
        native.as_object_mut().unwrap().remove("spawn_id");
        let err = std::panic::catch_unwind(|| {
            crate::native_adopt::assert_birth_superset(&native, &spawned)
        })
        .unwrap_err();
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(msg.contains("spawn_id"), "{msg}");
    }

    #[test]
    fn a_colliding_native_name_takes_a_numeric_suffix() {
        let (_d, home) = temp_home("suffix");
        seed_registry(&home, claude_row());
        let record = crate::launch_record::Record {
            launch: crate::launch_record::Launch {
                harness: "claude".into(),
                name: Some("w1".into()),
                node: None,
                slot: None,
                provider: None,
                cwd: "/repo".into(),
            },
            launcher_session: "lead-8".into(),
            launcher_harness: Some("claude".into()),
            launcher_cwd: Some("/repo".into()),
            launched_at_ms: 1_000,
            command: String::new(),
            child_session: Some("4444aaaa-2222-4333-8444-555566667777".into()),
            claimed_at_ms: Some(2_000),
        };
        let params = json!({
            "harness": "claude",
            "session_id": "4444aaaa-2222-4333-8444-555566667777",
            "launch_record": serde_json::to_value(&record).unwrap(),
        });
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["minted"], "native");
        let rows = read_rows(&home);
        let minted = rows
            .iter()
            .find(|r| r.spawned_by_session.as_deref() == Some("lead-8"))
            .expect("minted row");
        assert_eq!(minted.name, "w1-2");
    }

    /// A `spawn --promote --succeed` transfer leaves the manifest
    /// naming the stepping_down session, and the successor's levers (shape, term)
    /// read that manifest, so the successor holds with no levers. The successor's
    /// first session report is the arm point: the fill rebinds the manifest
    /// to the successor, including the harness when the row carries one (a claude
    /// predecessor's `harness: claude` must not outlive a codex successor). A
    /// later resume (related-slot fill) rewrites nothing.
    #[test]
    fn a_promoted_successors_first_report_binds_the_manifest_to_its_session() {
        let lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // FNO_SPACES_DIR, never FNO_AGENTS_HOME: the pin the manifest arm
        // resolves through, while the seeded registry stays an AgentsHome::at
        // path that the source-ahead schema guard never reads as the shared
        // file. Saved and restored around the whole test.
        let saved_spaces = std::env::var_os("FNO_SPACES_DIR");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("FNO_SPACES_DIR", dir.path().join("spaces"));
        let home = AgentsHome::at(dir.path().join("home"));
        std::fs::create_dir_all(home.root()).unwrap();
        let cwd = dir.path().join("repo");
        // A .git child makes space_dir resolve the repo itself, so the
        // manifest lands at a deterministic path whatever the ambient env.
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let successor = "01a10f07-d704-75a0-a460-7913efe8971b";
        let predecessor = "99473043-aaaa-4bbb-8ccc-ddddeeeeeeee";
        seed_registry(
            &home,
            json!({
                "name": "lead-wren", "cwd": cwd.display().to_string(),
                "status": "spawning", "created_at": "2026-10-05T20:06:00Z",
                "harness": "codex", "role_level": 2,
                "role_scope": "x-aaaa", "role_grantor": "vellum"
            }),
        );
        let leads = crate::paths::space_dir(&cwd).join("leads");
        std::fs::create_dir_all(&leads).unwrap();
        let manifest = leads.join("x-aaaa.md");
        std::fs::write(
            &manifest,
            format!(
                "---\nfno_id: 21fa9486-2bd8-426f-b7e8-8c376b60bf72\ncreated_at: 2026-10-05T20:31:14Z\n\
                 term: span:96h\nscope: x-aaaa\nshape: team\nharness: claude\n\
                 harness_session_id: {predecessor}\nrole_level: 2\nrole_scope: x-aaaa\n\
                 role_grantor: vellum\n---\n"
            ),
        )
        .unwrap();

        let params = report_params("codex", successor, json!({"agent_self": "lead-wren"}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(
            response_json(&resp)["result"]["stored"],
            true,
            "resp was {:?}",
            response_json(&resp)
        );
        let bound = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            bound.contains(&format!("harness_session_id: {successor}")),
            "{bound}"
        );
        // The harness rides with the id: the row's own harness wins, and the
        // predecessor's `harness: claude` cannot outlive the succession.
        assert!(bound.contains("harness: codex"), "{bound}");
        assert!(!bound.contains("harness: claude"), "{bound}");
        // Everything but the holder identity survives the rewrite.
        assert!(bound.contains("term: span:96h"), "{bound}");
        assert!(bound.contains("shape: team"), "{bound}");
        assert!(bound.contains("role_grantor: vellum"), "{bound}");
        assert_eq!(read_rows(&home)[0].status, AgentStatus::Live);

        // A resume fills the related slot and never rewrites the manifest.
        let resume = "0197bbbb-1234-7abc-9def-0123456789ab";
        let params = report_params("codex", resume, json!({"agent_self": "lead-wren"}));
        let resp = handle_session_report(&home, &emitter(&home), &req(params));
        assert_eq!(response_json(&resp)["result"]["related_filled"], true);
        let bound = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            bound.contains(&format!("harness_session_id: {successor}")),
            "{bound}"
        );
        match saved_spaces {
            Some(v) => std::env::set_var("FNO_SPACES_DIR", v),
            None => std::env::remove_var("FNO_SPACES_DIR"),
        }
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

    /// AC2-ERR, fail-soft leg: with no fno binary on the resolver seam, the
    /// pane-side lookup reads None and the report path proceeds unbound - a
    /// missing mux never blocks a session report.
    #[test]
    fn pane_side_lookup_fails_soft_without_fno() {
        let saved = std::env::var("FNO_LOOPCHECK_FNO_BIN").ok();
        std::env::set_var("FNO_LOOPCHECK_FNO_BIN", "/nonexistent/fno-under-test");
        let claim = pane_side_lookup("/w");
        match saved {
            Some(v) => std::env::set_var("FNO_LOOPCHECK_FNO_BIN", v),
            None => std::env::remove_var("FNO_LOOPCHECK_FNO_BIN"),
        }
        assert_eq!(claim, None);
    }
}

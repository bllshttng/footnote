//! `fno-agents-worker --muse-host <spec> <socket> <name>`: hold one Muse
//! session open over MSP and answer one JSON line per connection on a unix
//! socket under `<agents home>/muse/`:
//!
//! - `{"op":"status"}` names the session, the running turn and any approval.
//! - `{"op":"prompt","text":..,"envelope_id"?:..}` submits a `turn/start`
//!   (queued behind a running turn) and answers with the delivery receipt.
//! - `{"op":"answer","option":"allow_once"|"deny"}` decides the pending
//!   approval with one of the choices Muse offered.
//! - `{"op":"interrupt"}` stops the running turn.
//!
//! A delivery is proven only by the `userMessage` item whose `commandId` is
//! the one this host minted and whose text holds the envelope id. A
//! `turn/started` alone proves a turn, not the message; `turn/unqueued` means
//! the turn never runs. The receipt speaks the six delivery-state words.
//!
//! SIGINT or SIGTERM denies a pending approval, interrupts the running turn,
//! sends `session/leave` with `stop: true`, and closes `muse serve`'s stdin.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::msp::{mint_uuid_v7, Client, Incoming, MspError};
use crate::paths::AgentsHome;

/// How long a deliver waits for the proof before it answers with the state
/// it has (mail_inject.rs: DEFAULT_ATTEMPTS * DEFAULT_INTERVAL_MS).
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(10);
const OPEN_WAIT: Duration = Duration::from_secs(30);
const CALL_WAIT: Duration = Duration::from_secs(5);
/// How long an approval waits for `--answer` before the host denies it.
const ASK_WINDOW: Duration = crate::footnote_harness::thread::ASK_WINDOW;

static STOP: AtomicBool = AtomicBool::new(false);

/// What `dispatch_thread` hands the host: the serve argv and the session.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Spec {
    pub session_id: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub approval_mode: String,
    pub effort: Option<String>,
}

struct Sub {
    envelope: Option<String>,
    steered: bool,
    state: &'static str,
    turn_id: Option<String>,
    reason: Option<String>,
}

/// The delivery state of each submit, folded from MSP frames.
#[derive(Default)]
pub struct Tracker {
    subs: HashMap<String, Sub>,
    running: Option<String>,
    approval: Option<Value>,
}

fn settled(state: &str) -> bool {
    matches!(state, "accepted-new-turn" | "accepted-steer" | "failed")
}

impl Tracker {
    /// The frame for a submit went out: `injected`.
    pub fn submit(&mut self, command_id: &str, envelope: Option<&str>) {
        let sub = Sub {
            envelope: envelope.map(str::to_string),
            steered: false,
            state: "injected",
            turn_id: None,
            reason: None,
        };
        self.subs.insert(command_id.to_string(), sub);
    }

    /// The `turn/start` answer. `queued` parks the submit in Muse's queue;
    /// an error fails it with the error's kind.
    pub fn ack(&mut self, command_id: &str, result: &Result<Value, MspError>) {
        let running = self.running.clone();
        let Some(sub) = self.subs.get_mut(command_id) else {
            return;
        };
        if settled(sub.state) {
            return;
        }
        match result {
            Ok(r) => {
                sub.turn_id = r["turnId"].as_str().map(str::to_string);
                match r["disposition"].as_str() {
                    Some("queued") => {
                        sub.state = "parked";
                        sub.reason = running.map(|t| format!("parked-behind-turn {t}"));
                    }
                    Some("steered") => sub.steered = true,
                    _ => {}
                }
            }
            Err(e) => {
                sub.state = "failed";
                sub.reason = Some(e.to_string());
            }
        }
    }

    /// Fold one notification. Returns the submit it settled, if any.
    pub fn observe(&mut self, method: &str, params: &Value) -> Option<String> {
        let turn = params["turnId"].as_str().map(str::to_string);
        match method {
            "turn/started" => self.running = turn,
            "turn/completed" if self.running == turn => self.running = None,
            "approval/requested" | "approval/request" => self.approval = Some(params.clone()),
            "approval/resolved" => self.approval = None,
            "item/completed" | "item/updated" => {
                let item = &params["item"];
                if item["kind"] != "userMessage" {
                    return None;
                }
                let cid = item["commandId"].as_str()?;
                let text = item["text"].as_str().unwrap_or("");
                let sub = self.subs.get_mut(cid)?;
                let carries = sub.envelope.as_deref().is_none_or(|e| text.contains(e));
                if settled(sub.state) || !carries {
                    return None;
                }
                sub.state = if sub.steered {
                    "accepted-steer"
                } else {
                    "accepted-new-turn"
                };
                sub.turn_id = item["turnId"]
                    .as_str()
                    .map(str::to_string)
                    .or(sub.turn_id.take());
                return Some(cid.to_string());
            }
            "turn/unqueued" | "turn/retracted" => {
                if params["outcome"] == "declined" {
                    return None;
                }
                let cid = params["commandId"].as_str()?;
                let sub = self.subs.get_mut(cid)?;
                if settled(sub.state) {
                    return None;
                }
                sub.state = "failed";
                sub.reason = Some(
                    if method == "turn/unqueued" {
                        "reclaimed"
                    } else {
                        "retracted"
                    }
                    .into(),
                );
                return Some(cid.to_string());
            }
            _ => {}
        }
        None
    }

    /// Drop a submit whose receipt was answered and owes no late record.
    pub fn forget(&mut self, command_id: &str) {
        self.subs.remove(command_id);
    }

    pub fn is_settled(&self, command_id: &str) -> bool {
        self.subs.get(command_id).is_some_and(|s| settled(s.state))
    }

    /// The receipt for one submit. `delivered` stays for legacy callers and
    /// is true only for an accepted state.
    pub fn receipt(&self, command_id: &str) -> Value {
        let Some(sub) = self.subs.get(command_id) else {
            return json!({"state": "failed", "delivered": false, "reason": "unknown-command"});
        };
        json!({
            "state": sub.state,
            "delivered": sub.state.starts_with("accepted"),
            "command_id": command_id,
            "turn_id": sub.turn_id,
            "envelope_id": sub.envelope,
            "reason": sub.reason,
        })
    }
}

/// A raw `/name args` prompt becomes a skill part; anything else is text.
fn input_part(text: &str) -> Value {
    if let Some(rest) = text.strip_prefix('/') {
        let (selector, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if !selector.is_empty() && !selector.contains('/') {
            return json!({"type": "skill", "selector": selector, "arguments": args.trim()});
        }
    }
    json!({"type": "text", "text": text})
}

/// The choice whose decision matches the caller's answer, once-scoped first.
fn pick_choice(approval: &Value, allow: bool) -> Option<String> {
    let choices = approval["availableChoices"].as_array()?;
    let fits = |c: &&Value| {
        let decision = c["decision"].as_str().unwrap_or("");
        if allow {
            decision.starts_with("approved")
        } else {
            decision.starts_with("denied")
        }
    };
    choices
        .iter()
        .filter(fits)
        .min_by_key(|c| c["scope"] != "once")
        .and_then(|c| c["choiceId"].as_str().map(str::to_string))
}

struct Job {
    req: Value,
    reply: mpsc::Sender<Value>,
}

enum Pending {
    Turn(String),
    Reply(mpsc::Sender<Value>),
}

struct Session<'a> {
    client: &'a mut Client,
    spec: &'a Spec,
    tracker: Tracker,
    pending: HashMap<u64, Pending>,
    waiters: Vec<(String, mpsc::Sender<Value>, Instant)>,
    late: HashSet<String>,
    delivery_log: PathBuf,
    /// The pending approval's id and when this host first saw it.
    ask_since: Option<(String, Instant)>,
}

impl Session<'_> {
    fn job(&mut self, job: Job) -> Result<(), MspError> {
        let sid = self.spec.session_id.as_str();
        let reply = |v: Value| {
            let _ = job.reply.send(v);
        };
        match job.req["op"].as_str() {
            Some("status") => reply(json!({
                "ok": true,
                "session_id": sid,
                "busy": self.tracker.running.is_some(),
                "turn_id": self.tracker.running,
                "ask": self.tracker.approval,
            })),
            Some("prompt") => {
                let text = job.req["text"].as_str().unwrap_or("");
                if text.is_empty() {
                    reply(json!({"ok": false, "error": "the prompt carries no text"}));
                    return Ok(());
                }
                let envelope = job.req["envelope_id"].as_str();
                let cid = mint_uuid_v7();
                let mut params = json!({
                    "sessionId": sid,
                    "commandId": cid,
                    "input": [input_part(text)],
                    "ifBusy": "queue",
                });
                if let Some(env) = envelope {
                    params["displayText"] = json!(format!("[fno-mail {env}]"));
                }
                if let Some(effort) = &self.spec.effort {
                    params["reasoningEffort"] = json!(effort);
                }
                self.tracker.submit(&cid, envelope);
                let id = self.client.request("turn/start", params)?;
                self.pending.insert(id, Pending::Turn(cid.clone()));
                self.waiters
                    .push((cid, job.reply, Instant::now() + CONFIRM_WINDOW));
            }
            Some("answer") => {
                let allow = match job.req["option"].as_str() {
                    Some("allow_once") => true,
                    Some("deny") | Some("reject_once") => false,
                    _ => {
                        reply(json!({"ok": false, "error": "option must be allow_once or deny"}));
                        return Ok(());
                    }
                };
                if self.tracker.approval.is_none() {
                    reply(json!({"ok": false, "error": "no approval is pending"}));
                    return Ok(());
                }
                match self.decide(allow)? {
                    Some(id) => {
                        self.pending.insert(id, Pending::Reply(job.reply));
                    }
                    None => reply(json!({"ok": false, "error": "muse offered no matching choice"})),
                }
            }
            Some("interrupt") => {
                let id = self.client.request(
                    "turn/interrupt",
                    json!({"sessionId": sid, "commandId": mint_uuid_v7()}),
                )?;
                self.pending.insert(id, Pending::Reply(job.reply));
            }
            _ => reply(
                json!({"ok": false, "error": "op must be status, prompt, answer or interrupt"}),
            ),
        }
        Ok(())
    }

    fn frame(&mut self, frame: Incoming) {
        match frame {
            Incoming::Response { id, result } => match self.pending.remove(&id) {
                Some(Pending::Turn(cid)) => self.tracker.ack(&cid, &result),
                Some(Pending::Reply(tx)) => {
                    let _ = tx.send(match result {
                        Ok(r) => json!({"ok": true, "result": r}),
                        Err(e) => json!({"ok": false, "error": e.to_string()}),
                    });
                }
                None => {}
            },
            Incoming::Notification { method, params } | Incoming::Request { method, params } => {
                if let Some(cid) = self.tracker.observe(&method, &params) {
                    if self.late.remove(&cid) {
                        self.record_late(&cid);
                        self.tracker.forget(&cid);
                    }
                }
            }
        }
    }

    /// A submit that settled after its deliver answered: append the record
    /// a status reader shows as the late landing.
    fn record_late(&self, cid: &str) {
        let mut row = self.tracker.receipt(cid);
        row["ts"] = json!(crate::daemon::now_rfc3339_like());
        let line = format!("{row}\n");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.delivery_log);
        if let Ok(mut f) = file {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Answer every waiter whose submit settled or whose window closed.
    fn settle(&mut self) {
        let now = Instant::now();
        let mut keep = Vec::new();
        for (cid, tx, deadline) in std::mem::take(&mut self.waiters) {
            if self.tracker.is_settled(&cid) {
                let _ = tx.send(self.tracker.receipt(&cid));
                self.tracker.forget(&cid);
            } else if now >= deadline {
                let _ = tx.send(self.tracker.receipt(&cid));
                // Only a mail owes a late-landing record; drop anything else.
                if self.tracker.subs[&cid].envelope.is_some() {
                    self.late.insert(cid);
                } else {
                    self.tracker.forget(&cid);
                }
            } else {
                keep.push((cid, tx, deadline));
            }
        }
        self.waiters = keep;
    }

    /// Send `approval/decide` for the pending approval with the choice Muse
    /// offered for `allow`. None when nothing is pending or no choice fits.
    fn decide(&mut self, allow: bool) -> Result<Option<u64>, MspError> {
        let Some(approval) = self.tracker.approval.clone() else {
            return Ok(None);
        };
        let Some(choice) = pick_choice(&approval, allow) else {
            return Ok(None);
        };
        let params = json!({
            "sessionId": self.spec.session_id,
            "commandId": mint_uuid_v7(),
            "approvalId": approval["approvalId"],
            "requirementId": approval["currentRequirementId"],
            "choiceId": choice,
        });
        self.client.request("approval/decide", params).map(Some)
    }

    /// Deny an approval nobody answered within ASK_WINDOW, the footnote
    /// host's bound, so a lead who is away never stalls the session's queue.
    fn expire_ask(&mut self) -> Result<(), MspError> {
        let id = self
            .tracker
            .approval
            .as_ref()
            .and_then(|a| a["approvalId"].as_str());
        match (id, &self.ask_since) {
            (None, _) => self.ask_since = None,
            (Some(id), Some((seen, at))) if seen == id => {
                if at.elapsed() >= ASK_WINDOW {
                    let id = id.to_string();
                    eprintln!(
                        "muse-host: approval {id} unanswered for {}s; denied",
                        ASK_WINDOW.as_secs()
                    );
                    self.decide(false)?;
                    self.ask_since = Some((id, Instant::now()));
                }
            }
            (Some(id), _) => self.ask_since = Some((id.to_string(), Instant::now())),
        }
        Ok(())
    }

    /// Deny a pending approval, interrupt the running turn, leave.
    fn shut(&mut self) {
        let sid = self.spec.session_id.clone();
        let _ = self.decide(false);
        if self.tracker.running.is_some() {
            let params = json!({"sessionId": sid, "commandId": mint_uuid_v7()});
            let _ = self.client.call("turn/interrupt", params, CALL_WAIT);
        }
        let params = json!({"sessionId": sid, "commandId": mint_uuid_v7(), "stop": true});
        let _ = self.client.call("session/leave", params, CALL_WAIT);
    }
}

/// Serve jobs and frames until a stop signal or a lost session.
fn run(session: &mut Session, jobs: &mpsc::Receiver<Job>) -> Result<(), MspError> {
    while !STOP.load(Ordering::SeqCst) {
        while let Ok(job) = jobs.try_recv() {
            session.job(job)?;
        }
        if let Some(frame) = session.client.next(Duration::from_millis(50))? {
            session.frame(frame);
        }
        session.settle();
        session.expire_ask()?;
    }
    Ok(())
}

/// One request per connection: read a line, hand it to the session loop,
/// write its answer.
fn serve(listener: UnixListener, jobs: mpsc::Sender<Job>) {
    for stream in listener.incoming().map_while(Result::ok) {
        let jobs = jobs.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(CALL_WAIT));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                return;
            }
            let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
            let (tx, rx) = mpsc::channel();
            let reply = if jobs.send(Job { req, reply: tx }).is_ok() {
                rx.recv_timeout(CONFIRM_WINDOW + CALL_WAIT).unwrap_or_else(
                    |_| json!({"ok": false, "error": "the muse host did not answer"}),
                )
            } else {
                json!({"ok": false, "error": "the muse host is stopping"})
            };
            let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
        });
    }
}

/// Set STOP on SIGINT or SIGTERM.
fn watch_signals() {
    std::thread::spawn(|| {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async {
            let (Ok(mut int), Ok(mut term)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
            ) else {
                return;
            };
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
        });
        STOP.store(true, Ordering::SeqCst);
    });
}

/// The child's pid and start token, beside the socket, so a stop that had to
/// SIGKILL the host still reaps `muse serve`.
pub fn child_file(socket: &Path) -> PathBuf {
    socket.with_extension("child")
}

/// One request to a muse host socket, one JSON reply.
pub fn host_call(socket: &Path, req: &Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(CONFIRM_WINDOW + CALL_WAIT * 2));
    let _ = stream.set_write_timeout(Some(CALL_WAIT));
    (&stream)
        .write_all(format!("{req}\n").as_bytes())
        .map_err(|e| format!("muse host write: {e}"))?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| format!("muse host read: {e}"))?;
    serde_json::from_str(reply.trim()).map_err(|e| format!("muse host reply: {e}"))
}

fn open(spec: &Spec, name: &str) -> Result<Client, MspError> {
    let (bin, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| MspError::Closed("empty serve argv".into()))?;
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(&spec.cwd);
    // A launcher that updates itself would swap the binary under a live host.
    cmd.env("MUSE_NO_AUTO_UPDATE", "1");
    crate::claims::stamp_command_env(&mut cmd, Some(name), "muse", Some(&spec.session_id));
    let mut client = Client::spawn(cmd)?;
    client.initialize(OPEN_WAIT)?;
    let started = client.call(
        "session/start",
        json!({
            "commandId": mint_uuid_v7(),
            "sessionId": spec.session_id,
            "workspaceRoot": spec.cwd,
            "approvalMode": spec.approval_mode,
        }),
        OPEN_WAIT,
    )?;
    let got = started["session"]["sessionId"].as_str().unwrap_or("");
    if got != spec.session_id {
        return Err(MspError::Other {
            kind: "sessionMismatch".into(),
            message: format!("muse opened session {got}, not {}", spec.session_id),
        });
    }
    Ok(client)
}

/// Mark the row Exited once the host is done.
fn mark_exited(name: &str) {
    let home = AgentsHome::from_env();
    let _ = crate::state::update_registry(&home.registry_json(), |reg| {
        let Some(e) = reg.find_mut(name) else {
            return false;
        };
        e.status = crate::AgentStatus::Exited;
        e.exited_at = Some(crate::daemon::now_rfc3339_like());
        true
    });
}

pub fn run_host(spec_path: &Path, socket: &Path, name: &str) -> i32 {
    let spec: Spec = match std::fs::read_to_string(spec_path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("muse-host: {}: {e}", spec_path.display());
            return 2;
        }
    };
    let _ = std::fs::remove_file(spec_path);
    let mut client = match open(&spec, name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("muse-host: the session did not open: {e}");
            return 2;
        }
    };
    let _ = std::fs::remove_file(socket);
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("muse-host: {}: {e}", socket.display());
            client.close(CALL_WAIT);
            return 2;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    let pid = client.pid();
    let start = crate::daemon::process_start_time(pid);
    let _ = std::fs::write(
        child_file(socket),
        json!({"pid": pid, "start": start}).to_string(),
    );
    watch_signals();
    let (tx, jobs) = mpsc::channel();
    std::thread::spawn(move || serve(listener, tx));
    let mut session = Session {
        client: &mut client,
        spec: &spec,
        tracker: Tracker::default(),
        pending: HashMap::new(),
        waiters: Vec::new(),
        late: HashSet::new(),
        ask_since: None,
        delivery_log: socket.with_file_name(format!("{}.delivery.jsonl", spec.session_id)),
    };
    let ran = run(&mut session, &jobs);
    let _ = std::fs::remove_file(socket);
    let code = match ran {
        Ok(()) => {
            session.shut();
            0
        }
        Err(e) => {
            eprintln!("muse-host: session lost: {e}");
            1
        }
    };
    client.close(CALL_WAIT);
    let _ = std::fs::remove_file(child_file(socket));
    let stderr = client.stderr_text();
    if !stderr.is_empty() {
        eprintln!("muse-host: muse stderr:\n{stderr}");
    }
    mark_exited(name);
    code
}

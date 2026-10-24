//! `-H footnote --substrate thread`: one fnh session held open over ACP
//! stdio by an fno-owned host (`fno-agents-worker --footnote-host`). The host
//! runs `footnote acp <spec.json>`, keeps its pipes, and answers one JSON
//! line per connection on a unix socket under `<agents home>/fnh/`:
//!
//! - `{"op":"status"}` names the session, whether a turn runs, and any ask.
//! - `{"op":"prompt","text":..,"meta":..}` queues a `session/prompt`. Mail
//!   rides `meta` as `{"footnote/origin":{"kind":"mail",..}}`.
//! - `{"op":"answer","option":"allow_once"|"reject_once"}` answers the
//!   pending `session/request_permission`.
//!
//! SIGINT (what `fno agents stop` sends the row's pid group) rejects a
//! pending ask, sends `session/cancel`, and closes fnh's stdin. The socket
//! stays out of `mux/threads/`: stop and mail read a socket there as a pane
//! keeper and would speak the keeper protocol to it.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{outcome, resolve_footnote_bin, transcript, INSTALL_HINT};
use crate::acp_stdio::{
    session_new_params, AcpError, AcpSession, PermissionAnswerer, PermissionPolicy,
    FOOTNOTE_PROFILE,
};
use crate::claude_ask::AskOutcome;
use crate::footnote_transcript::LaunchSpec;
use crate::paths::AgentsHome;
use crate::protocol::{ErrorCode, Request, Response};
use crate::state::{load_registry, update_registry, RegistryEntry};

/// fnh rejects an ask nobody answers after 300 s (fnh `src/acp_agent.rs`
/// `ASK_TIMEOUT`). The host gives up first and rejects; neither side allows.
pub const ASK_WINDOW: Duration = Duration::from_secs(290);
const READY_WAIT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

static STOP: AtomicBool = AtomicBool::new(false);

struct Prompt {
    text: String,
    meta: Option<Value>,
}

struct Ask {
    request: Value,
    answer: mpsc::Sender<PermissionPolicy>,
}

#[derive(Default)]
pub struct Host {
    queue: Mutex<VecDeque<Prompt>>,
    wake: Condvar,
    busy: AtomicBool,
    gone: AtomicBool,
    /// Wakes the socket loop when fnh's session is lost.
    gone_wake: tokio::sync::Notify,
    ask: Mutex<Option<Ask>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The answerer the ACP session consults on each ask: it parks the ask where
/// an `answer` request finds it and waits up to `window`, then rejects.
pub fn ask_answerer(host: &Arc<Host>, window: Duration) -> PermissionAnswerer {
    let host = Arc::clone(host);
    Box::new(move |request: &Value| {
        let (tx, rx) = mpsc::channel();
        *lock(&host.ask) = Some(Ask {
            request: request.clone(),
            answer: tx,
        });
        let policy = rx
            .recv_timeout(window)
            .unwrap_or(PermissionPolicy::RejectOnce);
        lock(&host.ask).take();
        policy
    })
}

impl Host {
    /// One socket request's reply. `sid` is the session the host holds.
    pub fn reply(&self, sid: &str, req: &Value) -> Value {
        match req["op"].as_str() {
            Some("status") => json!({
                "ok": true,
                "session_id": sid,
                "busy": self.busy.load(Ordering::SeqCst),
                "queued": lock(&self.queue).len(),
                "ask": lock(&self.ask).as_ref().map(|a| a.request.clone()),
            }),
            Some("prompt") => {
                let text = req["text"].as_str().unwrap_or("");
                if text.is_empty() {
                    return json!({"ok": false, "error": "the prompt carries no text"});
                }
                let meta = req.get("meta").filter(|m| m.is_object()).cloned();
                let mut queue = lock(&self.queue);
                queue.push_back(Prompt {
                    text: text.to_string(),
                    meta,
                });
                self.wake.notify_all();
                json!({"ok": true, "queued": queue.len(), "busy": self.busy.load(Ordering::SeqCst)})
            }
            Some("answer") => {
                let policy = match req["option"].as_str() {
                    Some("allow_once") => PermissionPolicy::AllowOnce,
                    Some("reject_once") => PermissionPolicy::RejectOnce,
                    _ => {
                        return json!({"ok": false, "error": "option must be allow_once or reject_once"})
                    }
                };
                match lock(&self.ask).take() {
                    Some(ask) => {
                        let _ = ask.answer.send(policy);
                        json!({"ok": true})
                    }
                    None => json!({"ok": false, "error": "no permission ask is pending"}),
                }
            }
            _ => json!({"ok": false, "error": "op must be status, prompt or answer"}),
        }
    }

    /// Reject the pending ask, if any, so a turn waiting on it moves on.
    fn reject_pending(&self) {
        if let Some(ask) = lock(&self.ask).take() {
            let _ = ask.answer.send(PermissionPolicy::RejectOnce);
        }
    }
}

/// Run queued prompts one at a time: fnh serves one prompt per connection
/// at a time, and two requests in flight would steal each other's replies.
fn work(session: Arc<AcpSession>, host: Arc<Host>) {
    loop {
        let next = {
            let mut queue = lock(&host.queue);
            loop {
                if STOP.load(Ordering::SeqCst) || host.gone.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(p) = queue.pop_front() {
                    break p;
                }
                queue = host
                    .wake
                    .wait_timeout(queue, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        host.busy.store(true, Ordering::SeqCst);
        let ran = session.prompt_with_meta(&next.text, next.meta);
        host.busy.store(false, Ordering::SeqCst);
        session.take_notifications();
        match ran {
            Ok(_) => {}
            // The turn ended badly but the session is still open.
            Err(e @ (AcpError::Protocol { .. } | AcpError::NoPositiveResult { .. })) => {
                eprintln!("footnote-host: turn failed: {e}");
            }
            Err(e) => {
                eprintln!("footnote-host: session lost: {e}");
                host.gone.store(true, Ordering::SeqCst);
                host.gone_wake.notify_one();
                return;
            }
        }
    }
}

/// Answer socket requests until SIGINT, SIGTERM or a lost session.
fn serve_until_stop(listener: UnixListener, host: &Host, sid: &str) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::signal::unix::{signal, SignalKind};
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return eprintln!("footnote-host: no runtime: {e}"),
    };
    rt.block_on(async {
        let _ = listener.set_nonblocking(true);
        let listener = tokio::net::UnixListener::from_std(listener);
        let (int, term) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        );
        let (listener, mut int, mut term) = match (listener, int, term) {
            (Ok(l), Ok(i), Ok(t)) => (l, i, t),
            _ => return eprintln!("footnote-host: the socket or signal setup failed"),
        };
        loop {
            let stream = tokio::select! {
                _ = int.recv() => break,
                _ = term.recv() => break,
                _ = host.gone_wake.notified() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => stream,
                    Err(_) => continue,
                },
            };
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let mut reader = tokio::io::BufReader::new(read);
            let read = tokio::time::timeout(CALL_TIMEOUT, reader.read_line(&mut line)).await;
            if !matches!(read, Ok(Ok(_))) {
                continue;
            }
            let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
            let mut out = host.reply(sid, &req).to_string();
            out.push('\n');
            let _ = write.write_all(out.as_bytes()).await;
        }
    });
}

/// `fno-agents-worker --footnote-host <spec> <socket> <name>`: open the
/// session, serve the socket until SIGINT or fnh exits, mark the row.
pub fn run_host(spec_path: &Path, socket: &Path, name: &str) -> i32 {
    let spec: LaunchSpec = match std::fs::read_to_string(spec_path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("footnote-host: {}: {e}", spec_path.display());
            return 2;
        }
    };
    let argv = vec![
        resolve_footnote_bin().to_string_lossy().into_owned(),
        "acp".into(),
        spec_path.to_string_lossy().into_owned(),
    ];
    let opened = AcpSession::start_with_policy(
        &FOOTNOTE_PROFILE,
        argv,
        &spec.cwd,
        None,
        PermissionPolicy::RejectOnce,
    )
    .and_then(|s| {
        s.initialize()?;
        let sid = s.session_new(session_new_params(&spec.cwd, &[]))?;
        Ok((s, sid))
    });
    // fnh has read the spec, and it carries the endpoint key.
    let _ = std::fs::remove_file(spec_path);
    let (session, sid) = match opened {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("footnote-host: the session did not open: {e}");
            return 2;
        }
    };
    if sid != spec.fno_id {
        eprintln!(
            "footnote-host: fnh opened session {sid}, not the spec's {}",
            spec.fno_id
        );
        return 2;
    }
    let _ = std::fs::remove_file(socket);
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("footnote-host: {}: {e}", socket.display());
            return 2;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    if let Some(pid) = session.child_pid() {
        let start = crate::daemon::process_start_time(pid);
        let _ = std::fs::write(
            child_file(socket),
            json!({"pid": pid, "start": start}).to_string(),
        );
    }
    let session = Arc::new(session);
    let host = Arc::new(Host::default());
    session.set_answerer(ask_answerer(&host, ASK_WINDOW));
    let worker = {
        let (session, host) = (Arc::clone(&session), Arc::clone(&host));
        std::thread::spawn(move || work(session, host))
    };
    serve_until_stop(listener, &host, &sid);
    let _ = std::fs::remove_file(socket);
    STOP.store(true, Ordering::SeqCst);
    host.wake.notify_all();
    // A cancel that reaches fnh before the turn's prompt is cleared when the
    // turn starts, so it repeats until the turn returns.
    let cancel = session.cancel_handle().ok();
    while !worker.is_finished() {
        host.reject_pending();
        if let (true, Some(c)) = (host.busy.load(Ordering::SeqCst), &cancel) {
            let _ = c.cancel();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = worker.join();
    let stderr = session.stderr_text();
    // The last reference: dropping it closes fnh's stdin, and fnh ends the
    // session with its terminal record.
    drop(session);
    let _ = std::fs::remove_file(child_file(socket));
    if !stderr.is_empty() {
        eprintln!("footnote-host: fnh stderr:\n{stderr}");
    }
    let home = AgentsHome::from_env();
    let record = transcript::transcript_file(&spec.session_dir, &spec.fno_id);
    let ledger = crate::paths_cli::ledger_json(&spec.cwd);
    super::mark_row(&home, name, &record, &spec, &ledger);
    0
}

/// fnh's pid and start token, beside the host socket. A host the stop had to
/// SIGKILL never closed fnh, which leads its own group, so the stop reaps it.
fn child_file(socket: &Path) -> PathBuf {
    socket.with_extension("child")
}

/// Stop a footnote row's pid group. For a thread row, then make sure the
/// host's fnh child is gone too, or it keeps running and holds the session.
pub(crate) async fn stop_row(pid: u32, entry: &RegistryEntry) -> bool {
    let child = host_socket(entry).map(|s| child_file(&s));
    stop_host(pid, entry.pid_start_time, child).await
}

/// Stop a host's pid group, then SIGKILL the group of the child named in
/// `child_file` if it outlived its host. Shared by the footnote and muse
/// thread hosts, which both write that file beside their socket.
pub(crate) async fn stop_host(pid: u32, started: Option<u64>, child_file: Option<PathBuf>) -> bool {
    use crate::daemon::{pid_gone_within, pid_is_ours};
    let stopped = super::stop_confirmed(pid, started).await;
    let Some(file) = child_file else {
        return stopped;
    };
    let child = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            Some((
                u32::try_from(v["pid"].as_u64()?).ok()?,
                v["start"].as_u64()?,
            ))
        });
    if let Some((child, start)) = child {
        let gone = pid_gone_within(child, Some(start), Duration::from_secs(2)).await;
        if !gone && pid_is_ours(child, Some(start)) {
            // SAFETY: the child leads its own group (fnh: FOOTNOTE_PROFILE.own_group;
            // muse serve: process_group(0)), and pid_is_ours rejected pid 0, 1
            // and a recycled pid.
            unsafe { libc::killpg(child as libc::pid_t, libc::SIGKILL) };
        }
        let _ = std::fs::remove_file(&file);
    }
    stopped
}

/// One request to a host socket, one JSON reply.
pub fn host_call(socket: &Path, req: &Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(CALL_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CALL_TIMEOUT));
    let mut line = req.to_string();
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .map_err(|e| format!("footnote host write: {e}"))?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| format!("footnote host read: {e}"))?;
    serde_json::from_str(reply.trim()).map_err(|e| format!("footnote host reply: {e}"))
}

/// The host socket of a footnote thread row, when the row has one.
pub fn host_socket(entry: &RegistryEntry) -> Option<PathBuf> {
    if entry.harness_name() != "footnote" || entry.substrate.as_deref() != Some("thread") {
        return None;
    }
    entry.messaging_socket_path.as_ref().map(PathBuf::from)
}

fn exists_on_path(bin: &Path) -> bool {
    if bin.components().count() > 1 {
        return bin.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// `fno agents spawn -H footnote --substrate thread`: start the host, wait
/// until it serves the session, then register the row with its pid.
pub fn dispatch_thread(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    params: &Value,
) -> AskOutcome {
    if let Err(msg) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return outcome(2, String::new(), format!("{msg}\n"));
    }
    match load_registry(&home.registry_json()) {
        Ok(reg) if reg.find(name).is_some() => {
            let msg = format!("agent {name} already exists; use 'fno agents rm {name}' first\n");
            return outcome(2, String::new(), msg);
        }
        Err(e) => return outcome(12, String::new(), format!("registry read failed: {e}\n")),
        Ok(_) => {}
    }
    let bin = resolve_footnote_bin();
    if !exists_on_path(&bin) {
        let why = format!(
            "footnote binary not found at {}; {INSTALL_HINT}\n",
            bin.display()
        );
        return outcome(2, String::new(), why);
    }
    let node = params.get("node").and_then(Value::as_str);
    let mode = params.get("permission_mode").and_then(Value::as_str);
    let written = super::write_pane_spec(cwd, model.unwrap_or(""), node, mode, None).and_then(
        |(path, sid)| {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let spec: LaunchSpec = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            Ok((path, sid, spec))
        },
    );
    let (spec_path, sid, spec) = match written {
        Ok(w) => w,
        Err(e) => return outcome(2, String::new(), format!("{e}\n")),
    };
    let record = transcript::transcript_file(&spec.session_dir, &sid);
    let host_dir = home.root().join("fnh");
    let socket = host_dir.join(format!("{}.sock", &sid[..8]));
    let log = host_dir.join(format!("{sid}.log"));
    if let Err(e) = std::fs::create_dir_all(&host_dir) {
        return outcome(12, String::new(), format!("{}: {e}\n", host_dir.display()));
    }
    let log_file = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => return outcome(12, String::new(), format!("{}: {e}\n", log.display())),
    };
    let mut cmd = Command::new(crate::daemon::resolve_worker_bin());
    cmd.arg("--footnote-host")
        .args([&spec_path, &socket])
        .arg(name)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(
            log_file
                .try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        )
        .stderr(Stdio::from(log_file));
    for var in crate::claims::AMBIENT_IDENTITY_NAMES {
        cmd.env_remove(var);
    }
    // SAFETY: setsid is async-signal-safe; it runs between fork and exec. The
    // host leads its own group, which the stop command signals.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&spec_path);
            return outcome(
                12,
                String::new(),
                format!("cannot start the footnote host: {e}\n"),
            );
        }
    };
    let deadline = Instant::now() + READY_WAIT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let _ = std::fs::remove_file(&spec_path);
            let tail = std::fs::read_to_string(&log).unwrap_or_default();
            let why =
                format!("the footnote host exited ({status}) before the session opened:\n{tail}");
            return outcome(2, String::new(), why);
        }
        let status = host_call(&socket, &json!({"op": "status"}));
        if status.is_ok_and(|s| s["session_id"] == sid.as_str()) {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&spec_path);
            let why = format!(
                "the footnote host did not serve its session within {}s; log: {}\n",
                READY_WAIT.as_secs(),
                log.display()
            );
            return outcome(12, String::new(), why);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let pid = child.id();
    let entry = RegistryEntry {
        name: name.to_string(),
        short_id: sid.clone(),
        provider: Some("footnote".into()),
        harness: Some("footnote".into()),
        substrate: Some("thread".into()),
        session_id: Some(sid.clone()),
        harness_session_id: Some(sid.clone()),
        messaging_socket_path: Some(socket.to_string_lossy().into_owned()),
        requested_model: Some(spec.model.clone()),
        requested_permission_mode: spec.permission_mode.clone(),
        route_provider_id: spec.endpoint.provider_id.clone(),
        node: spec.node.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        origin: Some("spawn".into()),
        status: crate::AgentStatus::Live,
        pid: Some(pid),
        pid_start_time: crate::daemon::process_start_time(pid),
        created_at: crate::daemon::now_rfc3339_like(),
        log_path: Some(record.to_string_lossy().to_string()),
        account_record_id: Some("default".into()),
        ..RegistryEntry::new(Some(sid.clone()), crate::spawn_lineage::ambient_lineage())
    };
    let registered = update_registry(&home.registry_json(), |reg| {
        if reg.find(name).is_some() {
            return false;
        }
        reg.entries.push(entry.clone());
        true
    });
    if !matches!(registered, Ok(true)) {
        // SAFETY: the host leads its own group (setsid above).
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGINT) };
        return match registered {
            Ok(_) => outcome(2, String::new(), format!("agent {name} already exists\n")),
            Err(e) => outcome(12, String::new(), format!("registry write failed: {e}\n")),
        };
    }
    let seed = (!message.is_empty())
        .then(|| host_call(&socket, &json!({"op": "prompt", "text": message})));
    let receipt = json!({
        "ok": true,
        "name": name,
        "session_id": sid,
        "substrate": "thread",
        "host_pid": pid,
        "socket": socket,
        "log_path": record,
        "host_log": log,
        "seed": seed.map(|s| s.unwrap_or_else(|e| json!({"ok": false, "error": e}))),
    });
    outcome(0, format!("{receipt}\n"), String::new())
}

/// `agent.deliver`, the RPC the Python mail send calls for every harness but
/// claude and opencode. A footnote thread row gets the mail as a
/// `session/prompt` whose origin names the sender's full session id and the
/// message id; any other row answers `delivered: false`.
pub fn deliver_rpc(home: &AgentsHome, req: &Request) -> Response {
    let name = req.params["name"].as_str().unwrap_or("");
    let body = req.params["body"].as_str().unwrap_or("");
    let from = req.params["from_name"].as_str().unwrap_or("");
    let reg = match load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => return Response::err(req.id, ErrorCode::Internal, e.to_string()),
    };
    let Some(entry) = reg.find(name) else {
        return Response::ok(
            req.id,
            json!({"delivered": false, "reason": "no-such-agent"}),
        );
    };
    let Some(socket) = host_socket(entry) else {
        return Response::ok(
            req.id,
            json!({"delivered": false, "reason": "no-deliver-lane"}),
        );
    };
    // The RPC names the sender; its row holds the full id.
    let sender = reg
        .find_name_or_full_session_id(from)
        .and_then(|e| {
            e.harness_session_id
                .clone()
                .or_else(|| e.session_id.clone())
        })
        .unwrap_or_else(|| from.to_string());
    let origin = json!({"footnote/origin": {
        "kind": "mail",
        "sender_session_id": sender,
        "message_id": crate::mail_header::delivered_msg_id(body),
    }});
    let reply = host_call(
        &socket,
        &json!({"op": "prompt", "text": body, "meta": origin}),
    );
    Response::ok(
        req.id,
        match reply {
            Ok(r) if r["ok"] == true => json!({"delivered": true, "queued": r["queued"]}),
            Ok(r) => json!({"delivered": false, "reason": r["error"]}),
            Err(e) => {
                json!({"delivered": false, "reason": format!("footnote-host-unreachable: {e}")})
            }
        },
    )
}

/// `fno agents ask <name> [<message>] [--answer allow_once|deny]` on a live
/// footnote thread row: the message queues as a prompt, the answer resolves
/// the pending permission ask. None when the row has no reachable host, so
/// the caller falls back to a headless resume.
pub fn maybe_ask_host(entry: &RegistryEntry, params: &Value) -> Option<i32> {
    let socket = host_socket(entry)?;
    let answer = params["answer"].as_str();
    let req = match answer {
        Some("allow_once") => json!({"op": "answer", "option": "allow_once"}),
        Some("deny") => json!({"op": "answer", "option": "reject_once"}),
        Some(other) => {
            eprintln!("fno-agents: --answer takes allow_once or deny, not {other:?}");
            return Some(2);
        }
        None => json!({"op": "prompt", "text": params["message"].as_str().unwrap_or("")}),
    };
    match host_call(&socket, &req) {
        Ok(reply) => {
            println!("{reply}");
            Some(if reply["ok"] == true { 0 } else { 2 })
        }
        Err(e) if answer.is_some() => {
            eprintln!(
                "fno-agents: {}: the footnote host is not reachable: {e}",
                entry.name
            );
            Some(2)
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests;

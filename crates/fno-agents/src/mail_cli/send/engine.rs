// Ported from the single-file draft of send.rs; split by question.
use super::lanes::*;
use super::resolve::*;
use super::*;

// ---------------------------------------------------------------------------
// The per-agent registry lock (agents/lock.py hold_agent_lock)
// ---------------------------------------------------------------------------

/// The lock file lives at `<registry-dir>/locks/<name>.lock`, the path Python's
/// `_agent_lock_path` resolved. A flock, polled to the timeout, the holder
/// JSON rewritten on win. Python revalidated the inode across a registry
/// replacement mid-wait; this port takes the flock at the resolved path once
/// and holds it, which misses the replace-mid-wait edge only.
pub(super) struct AgentLock {
    file: std::fs::File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl AgentLock {
    fn acquire(
        name: &str,
        timeout: f64,
        on_wait: Option<&dyn Fn()>,
    ) -> Result<AgentLock, LockTimeout> {
        let dir = AgentsHome::shared_registry_json();
        let dir = dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let path = dir.join("locks").join(format!("{name}.lock"));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let start = std::time::Instant::now();
        let mut on_wait_fired = false;
        loop {
            let handle = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .map_err(LockTimeout::io)?;
            use std::os::unix::io::AsRawFd as _;
            match unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } {
                0 => {
                    let _ = std::fs::write(&path, holder_json());
                    return Ok(AgentLock { file: handle, path });
                }
                _ => {
                    drop(handle);
                    if start.elapsed().as_secs_f64() >= timeout {
                        return Err(LockTimeout {
                            name: name.to_string(),
                            timeout,
                            holder: read_holder(&path),
                        });
                    }
                    if !on_wait_fired && on_wait.is_some() && start.elapsed().as_secs_f64() >= 1.0 {
                        if let Some(cb) = on_wait {
                            cb();
                        }
                        on_wait_fired = true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }
}

impl Drop for AgentLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd as _;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(super) struct LockTimeout {
    name: String,
    timeout: f64,
    holder: String,
}

impl LockTimeout {
    fn io(e: std::io::Error) -> Self {
        LockTimeout {
            name: String::new(),
            timeout: 0.0,
            holder: format!("lock file unusable: {e}"),
        }
    }
    fn holder_note(&self, past: bool) -> String {
        if self.holder.is_empty() {
            return String::new();
        }
        let _ = past;
        format!(" ({})", self.holder)
    }
}

pub(super) fn holder_json() -> String {
    let pid = std::process::id();
    format!("{{\"pid\": {pid}, \"acquired_at\": \"{}\"}}\n", now_iso())
}

pub(super) fn read_holder(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let doc: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    match doc.get("pid").and_then(Value::as_u64) {
        Some(pid) if process_alive(pid as u32) => {
            let acquired = doc
                .get("acquired_at")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("held by pid {pid} since {acquired}")
        }
        _ => String::new(),
    }
}

pub(super) fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || errno() == libc::EPERM
}

pub(super) fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Python `_queue_grace_seconds`: the grace window covers the inject budget,
/// floored at 2s and capped at the caller's timeout.
pub(super) fn queue_grace_seconds(lock_timeout: f64) -> f64 {
    let residual = MAIL_INJECT_LIVENESS_SCALED_TIMEOUT_S + LOCK_TIMEOUT_QUEUE_GRACE_SECONDS;
    LOCK_TIMEOUT_QUEUE_GRACE_SECONDS.max((residual - lock_timeout).min(lock_timeout))
}

// ---------------------------------------------------------------------------
// Provider check and sender provenance (dispatch.py / sender_provenance.py)
// ---------------------------------------------------------------------------

/// Python `select_provider`: a --harness mismatch with the row refuses.
pub(super) fn select_provider(
    name: &str,
    requested: Option<&str>,
    row_harness: Option<&str>,
) -> Result<(), (String, i32)> {
    if let Some(req) = requested {
        if !KNOWN_PROVIDERS.contains(&req) {
            return Err((
                format!(
                    "unknown provider {req:?}; supported: {}",
                    KNOWN_PROVIDERS.join(", ")
                ),
                2,
            ));
        }
    }
    match (row_harness, requested) {
        (Some(existing), Some(req)) if existing != req => Err((
            format!(
                "agent {name:?} is provider={existing}, refusing to follow-up as provider={req}"
            ),
            2,
        )),
        _ => Ok(()),
    }
}

/// Python `_resolve_sender_entry`: the sender's registry row by handle.
pub(super) fn resolve_sender_entry(rows: &[Value], from_name: &str) -> Option<Value> {
    crate::client_verbs::find_agent_entry(rows, from_name)
        .ok()
        .cloned()
}

/// Python `_proven_self_sender`: the ambient identity wins when it proves the
/// from_name is this session's own handle.
pub(super) fn proven_self_sender(from_name: &str) -> Option<(String, String)> {
    let (sid, harness) = own_identity()?;
    if crate::identity::canonical_handle(&sid) == from_name {
        Some((harness, sid))
    } else {
        None
    }
}

/// Python `_stamped_sender_provenance`: proven self, else the row's harness
/// and session id; a miss warns once and ships unproven.
pub(super) fn stamped_sender_provenance(
    sender_row: Option<&Value>,
    from_name: &str,
) -> (Option<String>, Option<String>) {
    if let Some((harness, sid)) = proven_self_sender(from_name) {
        return (Some(harness), Some(sid));
    }
    let from_harness = sender_row.and_then(|row| opt_str_field(row, "harness"));
    let from_session = sender_row.and_then(|row| opt_str_field(row, "harness_session_id"));
    if from_harness.is_none() && from_session.is_none() {
        emit_event("sender_provenance_unknown", json!({"from_name": from_name}));
        eprintln!(
            "warning: sender {:?} resolved to no registry row and no ambient identity; envelope provenance degrades to unknown",
            from_name
        );
    }
    (from_harness, from_session)
}

/// Python `_build_mail_ctx`: the envelope's sender context.
#[derive(Clone, Default)]
pub(super) struct MailCtx {
    from_: String,
    model: String,
    node: Option<String>,
    to: Option<String>,
    from_session: Option<String>,
    id: Option<String>,
    origin: Option<String>,
    harness: Option<String>,
    to_session: Option<String>,
    subject: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_mail_ctx(
    from_name: &str,
    from_session: Option<String>,
    from_harness: Option<String>,
    to: Option<String>,
    id: Option<String>,
    origin: Option<&str>,
    to_session: Option<String>,
    subject: Option<&str>,
) -> MailCtx {
    let from_ = match &from_session {
        Some(sid) => crate::identity::canonical_handle(sid),
        None => from_name.to_string(),
    };
    MailCtx {
        from_,
        model: resolve_self_model(),
        node: None,
        to,
        from_session,
        id,
        origin: origin.map(enforce_origin_floor).map(str::to_string),
        harness: from_harness,
        to_session,
        subject: subject
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

// ---------------------------------------------------------------------------
// Live transport children (mail-inject lanes)
// ---------------------------------------------------------------------------

/// Python `_run_mail_inject`: one mail-inject child, verdict classified, the
/// miss wait named as `waited-<n>s`.
pub(super) fn run_mail_inject_child(
    argv: Vec<String>,
    text: &str,
    timeout: f64,
    reasons: Option<&mut Vec<String>>,
) -> bool {
    let started = std::time::Instant::now();
    let captured = run_self_captured(argv, text, timeout);
    if captured.spawn_failed || captured.timed_out {
        if let Some(reasons) = reasons {
            reasons.push("probe-unavailable".to_string());
        }
        return false;
    }
    let Ok(receipt) = serde_json::from_str::<Value>(captured.stdout.trim()) else {
        if let Some(reasons) = reasons {
            reasons.push("unreadable".to_string());
        }
        return false;
    };
    let delivered = receipt
        .get("delivered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(reasons) = reasons {
        let reason = receipt
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .unwrap_or("unknown");
        reasons.push(reason.to_string());
        if !delivered {
            reasons.push(format!("waited-{:.0}s", started.elapsed().as_secs_f64()));
        }
    }
    delivered
}

pub(super) fn inject_argv(
    harness: &str,
    session: &str,
    origin: Option<&str>,
    sender: Option<&str>,
    delay_ms: Option<u64>,
) -> Vec<String> {
    let mut argv = vec![
        "mail-inject".to_string(),
        "--harness".to_string(),
        harness.to_string(),
        "--session".to_string(),
        session.to_string(),
    ];
    if let Some(origin) = origin.filter(|o| !o.is_empty()) {
        argv.push("--origin".to_string());
        argv.push(origin.to_string());
    }
    if let Some(sender) = sender.filter(|s| !s.is_empty()) {
        argv.push("--sender".to_string());
        argv.push(sender.to_string());
    }
    if let Some(ms) = delay_ms {
        argv.push("--enter-delay-ms".to_string());
        argv.push(ms.to_string());
    }
    argv
}

/// Python `_mail_inject_claude`: control.sock inject via the mail-inject child.
pub(super) fn mail_inject_claude(
    recipient: &str,
    wrapped: &str,
    reasons: Option<&mut Vec<String>>,
    harness: Option<&str>,
) -> bool {
    if delivery_policy_refusal(Some(recipient), Some(wrapped)) == Some(BUS_ONLY_POLICY.to_string())
    {
        if let Some(reasons) = reasons {
            reasons.push(BUS_ONLY_POLICY.to_string());
        }
        return false;
    }
    let delay = crate::mail_inject::enter_delay_for_harness(harness.unwrap_or("claude"));
    let argv = inject_argv("claude", recipient, None, None, Some(delay as u64));
    run_mail_inject_child(argv, wrapped, 60.0, reasons)
}

/// Python `_mail_inject_codex`: the app-server daemon thread lane.
pub(super) fn mail_inject_codex(
    thread_id: &str,
    wrapped: &str,
    reasons: Option<&mut Vec<String>>,
    origin: Option<&str>,
) -> bool {
    if delivery_policy_refusal(Some(thread_id), Some(wrapped)) == Some(BUS_ONLY_POLICY.to_string())
    {
        if let Some(reasons) = reasons {
            reasons.push(BUS_ONLY_POLICY.to_string());
        }
        return false;
    }
    let argv = inject_argv("codex", thread_id, origin, None, None);
    run_mail_inject_child(argv, wrapped, 60.0, reasons)
}

/// Python `_mail_inject_keeper`: the keeper socket lane; the hosted harness
/// names the lane and the settle-delay table row.
pub(super) fn mail_inject_keeper(
    recipient: &str,
    wrapped: &str,
    harness: &str,
    reasons: Option<&mut Vec<String>>,
) -> bool {
    if delivery_policy_refusal(Some(recipient), Some(wrapped)) == Some(BUS_ONLY_POLICY.to_string())
    {
        if let Some(reasons) = reasons {
            reasons.push(BUS_ONLY_POLICY.to_string());
        }
        return false;
    }
    let argv = inject_argv(harness, recipient, None, None, None);
    run_mail_inject_child(argv, wrapped, 60.0, reasons)
}

/// Python `_delivery_policy_refusal`: the hold gate, in-process.
pub(super) fn delivery_policy_refusal(target: Option<&str>, body: Option<&str>) -> Option<String> {
    let target = target?;
    let verdict = crate::mail_hold::gate(target, body, chrono::Utc::now());
    if verdict.deliver {
        return None;
    }
    Some(BUS_ONLY_POLICY.to_string())
}

/// Python `_lane_heal`: the codex pane-miss heal probe.
pub(super) fn lane_heal(session_id: &str) -> (String, Option<String>, Option<Value>) {
    let argv = vec![
        "mail-inject".to_string(),
        "--harness".to_string(),
        "codex".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--lane-heal".to_string(),
    ];
    let captured = run_self_captured(argv, "", 30.0);
    if captured.spawn_failed || captured.timed_out {
        return (
            "unmeasurable".to_string(),
            Some("spawn-failed".to_string()),
            None,
        );
    }
    let Ok(parsed) = serde_json::from_str::<Value>(captured.stdout.trim()) else {
        return (
            "unmeasurable".to_string(),
            Some("unparseable-output".to_string()),
            None,
        );
    };
    let verdict = parsed
        .get("verdict")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("unmeasurable")
        .to_string();
    let reason = parsed
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pane = parsed
        .get("pane")
        .filter(|value| value.is_object())
        .cloned();
    (verdict, reason, pane)
}

/// Python `wake_and_deliver`: revive via the native resume door with the
/// wrapped body as the waking message. The reason tokens are the door's own.
pub(super) fn wake_and_deliver(
    session_uuid: &str,
    wrapped: &str,
    cwd: Option<&str>,
) -> (bool, String) {
    if session_uuid.is_empty() {
        return (false, "no-session-uuid".to_string());
    }
    let mut argv = vec![
        "fno".to_string(),
        "agents".to_string(),
        "resume".to_string(),
        session_uuid.to_string(),
        "--message".to_string(),
        wrapped.to_string(),
    ];
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        argv.push("--cwd".to_string());
        argv.push(cwd.to_string());
    }
    let front = crate::mail_remote::fno_binary();
    let mut full = vec![front.to_string_lossy().into_owned()];
    full.extend(argv);
    let captured = run_captured(&full, "", true, 120.0);
    if captured.spawn_failed || captured.timed_out {
        return (false, "wake-unrouted".to_string());
    }
    if captured.status == Some(0) {
        let short = crate::identity::canonical_handle(session_uuid);
        return (true, short.chars().take(8).collect());
    }
    (false, "resume-failed".to_string())
}

/// Python `_wake_rung`: wake an already-resolved asleep session; the
/// asleep-to-live race retries the socket once.
pub(super) fn wake_rung(
    reachable: &ReachableSession,
    wrapped: &str,
) -> (bool, Option<String>, Option<String>) {
    if reachable.agent != "claude" {
        return (
            false,
            None,
            Some(format!("wake=unsupported-harness({})", reachable.agent)),
        );
    }
    let (delivered, detail) =
        wake_and_deliver(&reachable.session_id, wrapped, reachable.cwd.as_deref());
    if delivered {
        return (true, Some(detail), None);
    }
    if detail == "writer-possibly-live" || detail == "wake-already-in-flight" {
        let mut reasons = Vec::new();
        if mail_inject_claude(&reachable.session_id, wrapped, Some(&mut reasons), None) {
            return (true, None, None);
        }
    }
    (false, None, Some(format!("wake={detail}")))
}

// ---------------------------------------------------------------------------
// Daemon RPC (the switchboard and agent.deliver lanes)
// ---------------------------------------------------------------------------

/// Python `_daemon_rpc` / `rpc_roundtrip`: one JSON-RPC request to the
/// supervisor socket, exactly one attempt, `None` on any transport error.
/// A down daemon demotes the lane to the durable floor, never errors.
pub(super) fn daemon_rpc(method: &str, params: Value) -> Option<Value> {
    let home = AgentsHome::from_env();
    let request = crate::protocol::Request::new(1, method, params);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let response = rt
        .block_on(crate::client::call_if_running(&home, &request))
        .ok()?;
    match response.payload {
        crate::protocol::ResponsePayload::Ok(value) => Some(value),
        _ => None,
    }
}

/// Python `_switchboard_identity`.
pub(super) fn switchboard_identity(entry: &Value) -> Value {
    json!({
        "harness": entry.get("harness"),
        "session_id": entry.get("harness_session_id"),
        "short_id": entry.get("short_id"),
        "created_at": entry.get("created_at"),
    })
}

pub(super) fn is_codex_thread_entry(entry: &Value) -> bool {
    str_field(entry, "harness") == "codex"
        && (opt_str_field(entry, "host_mode")
            .as_deref()
            .unwrap_or("exec")
            == "interactive")
        && opt_str_field(entry, "short_id").is_none()
        && entry.get("mux").is_none()
}

/// Python `_switchboard_exchange`'s first hop: `agent.switchboard_v2`, the
/// synchronous single observed hop (relay `auto` defaults off). A None/miss
/// demotes to the next lane.
pub(super) fn switchboard_exchange(
    to_name: &str,
    from_name: &str,
    wrapped: &str,
    _relay_ctxs: Option<Value>,
    to_identity: Value,
    from_identity: Option<Value>,
) -> Option<bool> {
    let mut params = json!({
        "to": to_name,
        "from": from_name,
        "body": wrapped,
        "mirror": from_identity.is_some(),
        "recipient_identity": to_identity,
    });
    if let Some(from_identity) = from_identity {
        params["from_identity"] = from_identity;
    }
    let sb = daemon_rpc("agent.switchboard_v2", params)?;
    if sb.get("delivered").and_then(Value::as_bool) != Some(true)
        || sb.get("identity_verified").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    Some(true)
}

/// One registry row: the dispatch engine's view. Row shapes are the registry
/// JSON Values the shared loader returns.
pub(super) type Row = Value;

// Pane entry resolution (thread_force.py resolve_pane_entry twin)

pub(super) fn resolve_pane_entry(
    resolved: Option<&ReachableSession>,
    recipient: Option<&str>,
    token: Option<&str>,
) -> Option<Value> {
    let registry = AgentsHome::shared_registry_json();
    let rows = crate::client_verbs::load_registry_entries(&registry).ok()?;
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(r) = resolved {
        candidates.push(r.session_id.as_str());
    }
    if let Some(t) = token {
        candidates.push(t);
    }
    if let Some(rec) = recipient {
        candidates.push(rec);
    }
    for candidate in candidates {
        for row in rows.iter() {
            if opt_str_field(row, "harness_session_id").as_deref() == Some(candidate) {
                return Some(row.clone());
            }
        }
    }
    None
}

// Keeper-thread row check (cli.py _row_hosts_keeper_thread twin)

pub(super) fn row_hosts_keeper_thread(session_id: &str) -> bool {
    let registry = AgentsHome::shared_registry_json();
    let Ok(rows) = crate::client_verbs::load_registry_entries(&registry) else {
        return false;
    };
    for row in rows.iter() {
        if opt_str_field(row, "harness_session_id").as_deref() == Some(session_id) {
            let sock = opt_str_field(row, "messaging_socket_path").unwrap_or_default();
            return sock.contains("mux/threads/");
        }
    }
    false
}

// Reachability verdict off the shared probe (dispatch.py recipient_live twin)

pub(super) fn family1_probe_reachable(handle: &str) -> bool {
    match crate::truth_probe::family1_truth_probe(handle) {
        Some(probe) => probe.reachability.as_deref() == Some("reachable"),
        None => false,
    }
}

// The mux pane lane (dispatch.py _mux_pane_send twin, mail-callers only)

pub(super) fn mux_pane_send(entry: &Value, wrapped: &str) -> bool {
    let session = entry
        .pointer("/mux/session")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pane = entry
        .pointer("/mux/pane_id")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if session.is_empty() || pane < 0 {
        eprintln!("mux pane send refused: no pane ref on the row");
        return false;
    }
    if crate::mail_hold::gate(session, Some(wrapped), chrono::Utc::now()).deliver {
        // not bus-only: fall through to the send
    } else {
        eprintln!("mux pane send refused: recipient is bus-only by policy");
        return false;
    }
    // The gate Python's prepare() runs client-side: a submit into a pane
    // showing an option prompt dismisses the payload and picks the default.
    let front = crate::mail_remote::fno_binary();
    let pane_num = pane.to_string();
    let read_argv: Vec<String> = vec![
        front.to_string_lossy().into_owned(),
        "mux".to_string(),
        "pane".to_string(),
        "read".to_string(),
        pane_num.clone(),
        "--lines".to_string(),
        "40".to_string(),
    ];
    let mut gated = false;
    let captured = run_captured(&read_argv, "", false, 10.0);
    if captured.status == Some(0) {
        if let Ok(tail) = serde_json::from_str::<Value>(captured.stdout.trim()) {
            let text = tail.get("text").and_then(Value::as_str).unwrap_or("");
            gated = crate::session_truth::option_prompt_re().is_match(text);
        }
    }
    if gated {
        eprintln!(
            "mux pane {pane} send refused: pane is showing an option prompt; a submit would discard the payload"
        );
        return false;
    }
    let send_argv: Vec<String> = [
        front.to_string_lossy().into_owned(),
        "mux".to_string(),
        "pane".to_string(),
        "send".to_string(),
        pane_num,
        "--text".to_string(),
        wrapped.to_string(),
        "--source".to_string(),
        "mail".to_string(),
    ]
    .to_vec();
    let sent = run_captured(&send_argv, "", true, 60.0);
    sent.status == Some(0)
}

// The a2a first-use gate (dispatch.py _a2a_first_use_gate twin)

pub(super) fn a2a_first_use_gate() {
    use std::io::IsTerminal as _;
    // config.agents.a2a.auto default true; a failed read degrades safely to
    // observed mode (false), an absent key keeps the default.
    let auto = match crate::agents_config::config_lookup(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        &["agents", "a2a", "auto"],
    ) {
        Some(v) => v.as_bool().unwrap_or(false),
        None => true,
    };
    if !auto {
        return;
    }
    let marker = a2a_marker();
    if marker.exists() {
        return;
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !interactive {
        eprintln!(
            "fno-agents a2a: no TTY to confirm autonomous relay; applying the conservative fallback (autonomous relay OFF, single observed hop). Run `fno config set config.agents.a2a.auto true` to opt in."
        );
        return;
    }
    let mut answer = String::new();
    eprint!(
        "\na2a auto-relay is ON: an A<->B send runs up to 5 autonomous turns, which draws plan credit.\nKeep auto-relay on? [Y/n] "
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let _ = std::io::stdin().read_line(&mut answer);
    let keep = !matches!(answer.trim(), "n" | "N");
    if keep {
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, "answered\n");
    }
}

/// paths.state_runtime_file: the state/ subfolder is the runtime home and
/// the legacy root spelling reads while it exists, so the marker moves with
/// the same resolver Python used and no root-level write lands here.
fn a2a_marker() -> PathBuf {
    let root = state_dir();
    let legacy = root.join(".a2a-confirmed");
    if legacy.exists() {
        return legacy;
    }
    root.join("state").join("a2a-confirmed")
}

// ---------------------------------------------------------------------------
// dispatch_send's engine (dispatch.py twins)
// ---------------------------------------------------------------------------

/// Python `_registered_family1_state`: classify a registered recipient without
/// trusting its stored lifecycle.
pub(super) fn registered_family1_state(row: &Value) -> String {
    let session_id = opt_str_field(row, "harness_session_id")
        .or_else(|| opt_str_field(row, "cc_session_id"))
        .or_else(|| opt_str_field(row, "session_id"))
        .or_else(|| opt_str_field(row, "short_id"))
        .unwrap_or_default();
    if session_id.is_empty() {
        return "unknown".to_string();
    }
    match crate::truth_probe::family1_truth_probe(&session_id) {
        Some(probe) => probe.state,
        None => "unknown".to_string(),
    }
}

/// Python `_deliver_live`: one fire-and-forget live attempt over the row's
/// ref. The caller stages the body row and writes the durable fallback when
/// this returns false. `wrapped` arrives already enveloped.
pub(super) fn deliver_live(
    row: &Value,
    wrapped: &str,
    from_name: &str,
    sender_entry: Option<&Value>,
    reason_out: &mut Vec<String>,
    _family1_state: &str,
) -> bool {
    // Bus-only bounds EVERY live transport below, this fn's first gate.
    let policy = crate::mail_hold::gate(
        &opt_str_field(row, "name").unwrap_or_default(),
        Some(wrapped),
        chrono::Utc::now(),
    );
    if !policy.deliver {
        reason_out.push(BUS_ONLY_POLICY.to_string());
        return false;
    }
    let has_mux = row.get("mux").is_some_and(|m| m.is_object());
    if has_mux {
        let mut attempt = 1;
        while attempt <= 2 {
            let delivered = mux_pane_send(row, wrapped);
            if delivered {
                return true;
            }
            reason_out.push(format!("mux-send-failed-attempt-{attempt}:pre-submit"));
            if attempt == 2 || str_field(row, "status").is_empty() {
                break;
            }
            attempt += 1;
        }
        // Heal after the pane miss: a codex row whose pane is gone but whose
        // thread is loaded in the app-server rebinds to the thread lane.
        let sid = opt_str_field(row, "harness_session_id");
        if str_field(row, "harness") == "codex" {
            if let Some(sid) = sid {
                let (verdict, _reason, _pane) = lane_heal(&sid);
                if verdict == "rebound-thread" {
                    let registry = AgentsHome::shared_registry_json();
                    if let Ok(rows) = crate::client_verbs::load_registry_entries(&registry) {
                        let rebound = rows.iter().find(|r| {
                            opt_str_field(r, "harness_session_id").as_deref() == Some(sid.as_str())
                        });
                        if let Some(rebound) = rebound {
                            let fresh = opt_str_field(rebound, "harness_session_id").unwrap_or(sid);
                            return mail_inject_codex(&fresh, wrapped, Some(reason_out), None);
                        }
                    }
                }
            }
        }
        return false;
    }
    if is_codex_thread_entry(row) {
        let delivered = switchboard_exchange(
            &str_field(row, "name"),
            from_name,
            wrapped,
            None,
            switchboard_identity(row),
            sender_entry.map(switchboard_identity),
        );
        if delivered == Some(true) {
            return true;
        }
        reason_out.push("codex-thread-switchboard-miss".to_string());
        return false;
    }
    let sock = opt_str_field(row, "messaging_socket_path").unwrap_or_default();
    let lane_harness = str_field(row, "harness");
    let keeper_row = sock.contains("mux/threads/")
        && crate::harness_capabilities::HarnessContract::packaged()
            .ok()
            .and_then(|c| c.thread_lane(lane_harness).ok())
            .map(|lane| lane == "keeper")
            .unwrap_or(false);
    if keeper_row {
        if let Some(sid) = opt_str_field(row, "harness_session_id") {
            return mail_inject_keeper(&sid, wrapped, lane_harness, Some(reason_out));
        }
    }
    let route_harness = lane_harness;
    if route_harness != "claude" && route_harness != "opencode" {
        let result = daemon_rpc(
            "agent.deliver",
            json!({
                "name": str_field(row, "name"),
                "body": wrapped,
                "from_name": from_name,
            }),
        );
        match result {
            None => return false,
            Some(result) => {
                if result.get("delivered").and_then(Value::as_bool) == Some(true) {
                    return true;
                }
                let reason = result
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("unknown");
                eprintln!("fno-agents deliver demoted: {reason}; message queued durable");
                reason_out.push(reason.to_string());
                return false;
            }
        }
    }
    // The switchboard fast lane: the daemon probes B's worker socket, so a
    // claude peer that is NOT a live stream thread answers delivered=false and
    // falls through to the socket path below. Observed mode mirrors B's reply
    // when the sender row is in hand.
    let to_identity = switchboard_identity(row);
    let from_identity = sender_entry.map(switchboard_identity);
    a2a_first_use_gate();
    let exchange = switchboard_exchange(
        &str_field(row, "name"),
        from_name,
        wrapped,
        None,
        to_identity,
        from_identity,
    );
    if exchange == Some(true) {
        return true;
    }
    // Live inject over control.sock: the SOLE claude live lane.
    let recipient = opt_str_field(row, "harness_session_id")
        .or_else(|| opt_str_field(row, "short_id"))
        .or_else(|| opt_str_field(row, "mcp_channel_id"));
    match recipient {
        None => {
            reason_out.push("no-recipient".to_string());
            false
        }
        Some(recipient) => {
            mail_inject_claude(&recipient, wrapped, Some(reason_out), Some(lane_harness))
        }
    }
}

/// Python `_queue_durable_fallback`: the durable FALLBACK envelope, the same
/// helper the lock-timeout grace queue uses. Returns (msg_id, durable_to).
pub(super) fn queue_durable_fallback(
    row: &Value,
    message: &str,
    mail_ctx: &MailCtx,
    msg_id: &str,
    owner: &str,
    subject: Option<&str>,
) -> Result<(String, String), (String, i32)> {
    let sid = opt_str_field(row, "harness_session_id");
    let durable_recipient = match sid {
        Some(sid) => crate::identity::canonical_handle(&sid),
        None => {
            let name = str_field(row, "name");
            return Err((
                format!(
                    "cannot queue durable mail for {name:?}: registry row has no full harness session id; no durable envelope was written"
                ),
                12,
            ));
        }
    };
    let sender = mail_ctx.from_.clone();
    let sender_harness = mail_ctx.harness.clone();
    let sender_session = mail_ctx.from_session.clone();
    let input = json!({
        "mode": "wrap",
        "body": message,
        "from": sender,
        "harness": sender_harness,
        "to": durable_recipient,
        "id": msg_id,
        "from_session": sender_session,
        "origin": mail_ctx.origin,
        "subject": subject,
    });
    let registry_path = AgentsHome::shared_registry_json();
    let durable_body = crate::mail_envelope::render_at(&input, &registry_path)
        .unwrap_or_else(|_| message.to_string());
    let words = crate::backlog::style_check::word_count(message) as i64;
    let write = write_durable_thread(
        &durable_recipient,
        &mail_ctx.from_,
        "send",
        &durable_body,
        msg_id,
        Some("session"),
        opt_str_field(row, "harness").as_deref(),
        mail_ctx.harness.as_deref(),
        mail_ctx.from_session.as_deref(),
        Some(&mail_ctx.model),
        owner,
        Some(words),
        mail_ctx.origin.as_deref(),
        subject,
        None,
    );
    match write {
        Ok(_) => Ok((msg_id.to_string(), durable_recipient)),
        Err(exc) => {
            let tail = if exc.contains(NO_ENVELOPE_CLAUSE) {
                String::new()
            } else {
                format!("; {NO_ENVELOPE_CLAUSE}")
            };
            Err((format!("durable envelope write failed: {exc}{tail}"), 12))
        }
    }
}

/// Python `_stamp_after_delivery`: bump last_message_at (and status for a
/// hosted send) after delivery. Best-effort: a failure here cannot make the
/// send retryable, but it must stay visible on stderr.
pub(super) fn stamp_after_delivery(name: &str, delivery: &str, expected_identity: &str) {
    // The same verify-recipient-under-lock rule the queue path runs: an
    // owner change between resolve and stamp must refuse the write rather
    // than stamp a row that now names a different session.

    use std::io::Write as _;
    let registry = AgentsHome::shared_registry_json();
    let lock_dir = registry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("locks");
    let _ = std::fs::create_dir_all(&lock_dir);
    let lock_path = lock_dir.join("registry-stamp.lock");
    let lock = match lock_dir_acquire(&lock_path, 1.0) {
        Ok(lock) => lock,
        Err(_) => return,
    };
    let run = (|| -> Result<(), String> {
        let raw = std::fs::read_to_string(&registry).map_err(|e| e.to_string())?;
        let mut doc: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let rows = doc
            .get_mut("agents")
            .and_then(Value::as_array_mut)
            .ok_or("no agents array")?;
        let mut hit = false;
        let mut identity_changed = false;
        for row in rows.iter_mut() {
            if str_field(row, "name") == name {
                hit = true;
                if recipient_identity(row) != expected_identity {
                    identity_changed = true;
                    eprintln!(
                        "registry stamp failed after {delivery} delivery for {name:?}: recipient identity changed; delivery succeeded; do not retry"
                    );
                    continue;
                }
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("last_message_at".to_string(), json!(utc_now_iso()));
                    if delivery == "hosted" {
                        obj.insert("status".to_string(), json!("live"));
                    }
                }
            }
        }
        if !hit || identity_changed {
            return Ok(());
        }
        let rendered = doc.to_string();
        let tmp = registry.with_extension("json.tmp");
        {
            let mut fh = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            fh.write_all(rendered.as_bytes())
                .map_err(|e| e.to_string())?;
            fh.flush().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &registry).map_err(|e| e.to_string())
    })();
    let _ = lock_dir_release(lock);
    if let Err(exc) = run {
        eprintln!("registry stamp failed after {delivery} delivery for {name:?}: {exc}; delivery succeeded; do not retry");
    }
}

// ---------------------------------------------------------------------------
// dispatch_send (dispatch.py twin) and its result
// ---------------------------------------------------------------------------

pub(super) struct DispatchSendResult {
    pub(super) msg_id: String,
    pub(super) delivery: String,
    pub(super) reason: Option<String>,
    pub(super) durable_owner: Option<String>,
    pub(super) to: Option<String>,
    pub(super) to_harness: Option<String>,
    pub(super) to_session: Option<String>,
}

/// Python `_recipient_identity_key`: every field that can select or replace a
/// live recipient, as one fingerprint string.
pub(super) fn recipient_identity(row: &Value) -> String {
    let session_id =
        opt_str_field(row, "harness_session_id").or_else(|| opt_str_field(row, "session_id"));
    let session_key = session_id
        .as_deref()
        .map(crate::spawn_context::session_identity_key);
    let mux = row.get("mux");
    let mux_part = match mux {
        Some(m) => format!(
            "{}:{}",
            m.get("session").and_then(Value::as_str).unwrap_or(""),
            m.get("pane_id").and_then(Value::as_i64).unwrap_or(-1)
        ),
        None => String::new(),
    };
    [
        str_field(row, "name"),
        str_field(row, "harness"),
        session_key.unwrap_or_default().as_str(),
        opt_str_field(row, "short_id").unwrap_or_default().as_str(),
        opt_str_field(row, "mcp_channel_id")
            .unwrap_or_default()
            .as_str(),
        mux_part.as_str(),
        str_field(row, "created_at"),
    ]
    .join("\u{1f}")
}

/// Python `_validate_inputs` (name_is_address=True: no short-id shape check).
pub(super) fn validate_send_inputs(
    name: &str,
    message: &str,
    from_name: &str,
) -> Result<(), (String, i32)> {
    if name.is_empty() {
        return Err(("agent name must not be empty".to_string(), 2));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err((
            format!("agent name must not contain path separators or '..': {name:?}"),
            2,
        ));
    }
    if name.len() > 128 {
        return Err((format!("name must be <=128 chars (got {})", name.len()), 2));
    }
    for bad in ["\u{0}", "\n", "\r", "="] {
        if name.contains(bad) {
            return Err((
                format!("agent name {name:?} contains a forbidden character ({bad:?} would corrupt subprocess env injection)"),
                2,
            ));
        }
    }
    if message.trim().is_empty() {
        return Err(("message must be non-empty".to_string(), 2));
    }
    if from_name.is_empty() {
        return Err(("from-name must not be empty".to_string(), 2));
    }
    if from_name.len() > 128 {
        return Err((
            format!("from-name must be <=128 chars (got {})", from_name.len()),
            2,
        ));
    }
    Ok(())
}

/// The registry load + one dispatch-address resolution.
pub(super) fn load_and_resolve_target(
    requested_name: &str,
) -> Result<(Vec<Value>, Value), (String, i32)> {
    let registry = AgentsHome::shared_registry_json();
    let rows = match crate::client_verbs::load_registry_entries(&registry) {
        Ok(rows) => rows,
        Err(exc) => {
            emit_event(
                "agent_send_failed",
                json!({"stage": "registry-read", "name": requested_name}),
            );
            return Err((format!("registry read failed: {exc}"), 12));
        }
    };
    let existing = match crate::client_verbs::find_agent_entry(&rows, requested_name) {
        Ok(entry) => entry.clone(),
        Err(_) => {
            emit_event(
                "agent_send_failed",
                json!({"stage": "unknown-name", "name": requested_name}),
            );
            return Err((
                format!(
                    "unknown agent {requested_name:?}; spawn it first: fno agents spawn {requested_name} --harness <harness>"
                ),
                UNKNOWN_AGENT_EXIT,
            ));
        }
    };
    Ok((rows, existing))
}

/// Python `_load_and_resolve_target`'s foreign-live ambiguity check: a token
/// that also matches a live discovery session outside the registry identity
/// is ambiguous across sessions; refuse with the full-id hint.
pub(super) fn check_dispatch_ambiguity(
    requested_name: &str,
    existing: &Value,
    discovered: &mut Option<Vec<ReachableSession>>,
) -> Result<(), (String, i32)> {
    if discovered.is_none() {
        *discovered = Some(resolve_reachable(requested_name).hit.into_iter().collect());
    }
    let discovered = discovered.as_ref().expect("checked");
    let registry_id = opt_str_field(existing, "harness_session_id");
    let registry_key = (
        str_field(existing, "harness").to_string(),
        registry_id
            .as_deref()
            .map(crate::spawn_context::session_identity_key)
            .unwrap_or_default(),
    );
    let mut foreign: Vec<&ReachableSession> = discovered
        .iter()
        .filter(|s| {
            (
                s.agent.clone(),
                crate::spawn_context::session_identity_key(&s.session_id),
            ) != registry_key
        })
        .collect();
    foreign.sort_by(|a, b| {
        (a.agent.clone(), a.session_id.clone()).cmp(&(b.agent.clone(), b.session_id.clone()))
    });
    if foreign.is_empty() {
        return Ok(());
    }
    let label = registry_id.unwrap_or_else(|| str_field(existing, "name").to_string());
    let mut candidates = vec![format!(
        "{label} ({}, registry name={})",
        str_field(existing, "harness"),
        str_field(existing, "name")
    )];
    for session in foreign {
        candidates.push(format!(
            "{} ({}, live discovery)",
            session.session_id, session.agent
        ));
    }
    Err((
        format!(
            "token {requested_name:?} is ambiguous across {} sessions: {}. Disambiguate with the full session id.",
            candidates.len(),
            candidates.join(", ")
        ),
        2,
    ))
}

/// Python `dispatch_send`: the registered-agent lane, live-inject-first with
/// a durable fallback, the per-agent flock spanning the whole send. The
/// lock-timeout grace path queues durable (exit 0); sustained contention
/// exits 11 with nothing written.
#[allow(clippy::too_many_lines)]
pub(super) fn dispatch_send(
    requested_name: &str,
    message: &str,
    provider: Option<&str>,
    lock_timeout: f64,
    from_name: &str,
    origin: Option<&str>,
    subject: Option<&str>,
) -> Result<DispatchSendResult, (String, i32)> {
    validate_send_inputs(requested_name, message, from_name)?;
    if message.len() > 1024 * 1024 {
        return Err((
            format!(
                "message body exceeds maximum size (1 MiB); got {} bytes",
                message.len()
            ),
            2,
        ));
    }
    let (_, initial) = load_and_resolve_target(requested_name)?;
    let canonical_name = str_field(&initial, "name").to_string();
    let canonical_identity = recipient_identity(&initial);
    let mut discovered: Option<Vec<ReachableSession>> = None;
    check_dispatch_ambiguity(requested_name, &initial, &mut discovered)?;

    let on_wait = || {
        eprintln!("Waiting for agent {canonical_name:?} lock...");
    };
    let mut lock = match AgentLock::acquire(&canonical_name, lock_timeout, Some(&on_wait)) {
        Ok(lock) => lock,
        Err(timeout_err) => {
            return dispatch_send_lock_timeout(
                requested_name,
                &canonical_name,
                &canonical_identity,
                message,
                provider,
                from_name,
                origin,
                subject,
                timeout_err,
            );
        }
    };
    let outcome = dispatch_send_locked(
        requested_name,
        &canonical_name,
        &canonical_identity,
        message,
        provider,
        from_name,
        origin,
        subject,
        &mut lock,
        &mut discovered,
    );
    drop(lock);
    outcome
}

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_send_locked(
    requested_name: &str,
    canonical_name: &str,
    canonical_identity: &str,
    message: &str,
    provider: Option<&str>,
    from_name: &str,
    origin: Option<&str>,
    subject: Option<&str>,
    _lock: &mut AgentLock,
    discovered: &mut Option<Vec<ReachableSession>>,
) -> Result<DispatchSendResult, (String, i32)> {
    let (entries, existing) = load_and_resolve_target(requested_name)?;
    if str_field(&existing, "name") != canonical_name {
        return Err((
            format!(
                "agent address {requested_name:?} changed from {canonical_name:?} to {} while acquiring its lock; retry the send",
                str_field(&existing, "name")
            ),
            2,
        ));
    }
    if recipient_identity(&existing) != canonical_identity {
        return Err((
            format!(
                "agent address {requested_name:?} changed while acquiring its lock; recipient identity changed; retry the send"
            ),
            2,
        ));
    }
    check_dispatch_ambiguity(requested_name, &existing, discovered)?;
    let name = canonical_name;
    select_provider(
        name,
        provider,
        opt_str_field(&existing, "harness").as_deref(),
    )
    .map_err(|(msg, code)| (msg, code))?;
    let sender_row = resolve_sender_entry(&entries, from_name);
    let self_proof = proven_self_sender(from_name);
    let sender_row = match &self_proof {
        Some((_harness, sid)) => {
            let row_matches = sender_row
                .as_ref()
                .and_then(|r| opt_str_field(r, "harness_session_id"))
                .as_deref()
                == Some(sid.as_str());
            if row_matches {
                sender_row
            } else {
                None
            }
        }
        None => sender_row,
    };
    let (from_harness, from_session) = stamped_sender_provenance(sender_row.as_ref(), from_name);
    let msg_id = generate_msg_id();
    let durable_recipient = opt_str_field(&existing, "harness_session_id")
        .map(|sid| crate::identity::canonical_handle(&sid));
    let budget_recipient = durable_recipient
        .clone()
        .or_else(|| opt_str_field(&existing, "short_id"))
        .unwrap_or_else(|| name.to_string());
    let mail_ctx = build_mail_ctx(
        from_name,
        from_session.clone(),
        from_harness.clone(),
        durable_recipient
            .clone()
            .or_else(|| opt_str_field(&existing, "short_id")),
        Some(msg_id.clone()),
        origin,
        opt_str_field(&existing, "harness_session_id"),
        subject,
    );
    let reservation = reserve_send_budget(&mail_ctx.from_, &budget_recipient, message, &msg_id);
    if let Err((msg, code)) = &reservation {
        return Err((msg.clone(), *code));
    }
    let reservation = reservation.expect("checked above");

    let family1_state = registered_family1_state(&existing);
    let family1_live =
        family1_state == "working" || family1_state == "watching" || family1_state == "your-move";
    let durable_owner = classify_durable_owner(false, family1_live, !family1_live);
    let family1_attemptable = family1_live
        || family1_state == "unknown"
        || (str_field(&existing, "harness") == "claude" && existing.get("mux").is_none());

    let mut live_attempted = false;
    let mut delivery = "durable".to_string();
    let mut demotion_notice: Option<String> = None;
    let mut live_miss_reason: Option<String> = None;
    let mut live_reasons: Vec<String> = Vec::new();
    let _bus_only = delivery_policy_refusal(opt_str_field(&existing, "name").as_deref(), None)
        == Some(BUS_ONLY_POLICY.to_string());

    emit_event(
        "agent_send_started",
        json!({"name": name, "provider": str_field(&existing, "harness"), "msg_id": msg_id}),
    );
    let write_durable = |live_attempted: bool| -> Result<(), (String, i32)> {
        let queued = queue_durable_fallback(
            &existing,
            message,
            &mail_ctx,
            &msg_id,
            durable_owner,
            subject,
        );
        if queued.is_err() && !live_attempted {
            release_budget(&reservation);
        }
        queued.map(|_| ())
    };
    // Write-ahead: a recipient we will not attempt live is asleep and cannot
    // drain during a live window; write its durable placeholder first.
    if durable_recipient.is_some() && !family1_attemptable {
        write_durable(live_attempted)?;
    }
    if family1_attemptable {
        let recipient_for_row = durable_recipient
            .clone()
            .or_else(|| opt_str_field(&existing, "short_id"))
            .unwrap_or_default();
        let wrapped = crate::mail_envelope::render_at(
            &json!({
                "mode": "wrap",
                "body": message,
                "from": mail_ctx.from_,
                "harness": mail_ctx.harness,
                "node": mail_ctx.node,
                "to": mail_ctx.to,
                "id": mail_ctx.id,
                "from_session": mail_ctx.from_session,
                "to_session": mail_ctx.to_session,
                "origin": mail_ctx.origin,
                "subject": mail_ctx.subject,
            }),
            &AgentsHome::shared_registry_json(),
        )
        .unwrap_or_else(|_| message.to_string());
        let stage_ok = if recipient_for_row.is_empty() {
            true
        } else {
            match stage_before_live(
                &msg_id,
                &mail_ctx.from_,
                &recipient_for_row,
                &wrapped,
                from_harness.as_deref(),
                Some(str_field(&existing, "harness")),
                None,
                from_session.as_deref(),
                Some(&mail_ctx.model),
                Some("session"),
                Some(crate::backlog::style_check::word_count(message) as i64),
                mail_ctx.subject.as_deref(),
            ) {
                Ok(()) => true,
                Err(exc) => {
                    eprintln!("body row not staged; skipping live delivery: {exc}");
                    live_miss_reason = Some("stage-failed".to_string());
                    false
                }
            }
        };
        if stage_ok {
            live_attempted = true;
            let delivered = deliver_live(
                &existing,
                &wrapped,
                from_name,
                sender_row.as_ref(),
                &mut live_reasons,
                &family1_state,
            );
            if delivered {
                delivery = "hosted".to_string();
                let record = bus_envelope_row(
                    &msg_id,
                    &msg_id,
                    &mail_ctx.from_,
                    &recipient_for_row,
                    "send",
                    &wrapped,
                    &now_iso(),
                    None,
                    json!({}),
                    from_harness.as_deref(),
                    Some(str_field(&existing, "harness")),
                    from_session.as_deref(),
                    Some(&mail_ctx.model),
                    Some("session"),
                    Some(crate::backlog::style_check::word_count(message) as i64),
                    mail_ctx.to_session.as_deref(),
                    mail_ctx.subject.as_deref(),
                );
                if let Err(exc) = record_hosted_delivery(
                    &record,
                    Some(str_field(&existing, "harness")),
                    mail_ctx.to_session.as_deref(),
                ) {
                    eprintln!(
                        "delivery succeeded; receipt row failed; the staged bus row holds the body; do not retry: {exc}"
                    );
                }
            } else if !live_reasons.is_empty() {
                let mut reason = live_reasons.join(";");
                if existing.get("mux").is_some_and(|m| m.is_object()) {
                    reason = format!("{reason};owner={durable_owner}");
                }
                live_miss_reason = Some(reason);
            }
        }
    }
    if _bus_only {
        live_miss_reason = Some(BUS_ONLY_POLICY.to_string());
    }
    if delivery != "hosted" && (durable_recipient.is_none() || family1_attemptable) {
        write_durable(live_attempted)?;
    }
    if delivery == "durable" {
        if family1_live {
            let why = live_miss_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            demotion_notice = Some(format!(
                "live delivery failed for {name:?}{why}; message queued durable ({msg_id})"
            ));
        }
    }
    emit_event(
        "agent_send_done",
        json!({
            "name": name,
            "provider": str_field(&existing, "harness"),
            "msg_id": msg_id,
            "delivery": delivery,
            "reason": live_miss_reason,
        }),
    );
    if let Some(notice) = demotion_notice {
        eprintln!("{notice}");
    }
    stamp_after_delivery(name, &delivery, canonical_identity);
    let durable_queued = delivery == "durable";
    Ok(DispatchSendResult {
        msg_id,
        delivery,
        reason: live_miss_reason,
        durable_owner: if durable_queued {
            Some(durable_owner.to_string())
        } else {
            None
        },
        to: durable_recipient.or_else(|| opt_str_field(&existing, "short_id")),
        to_harness: opt_str_field(&existing, "harness"),
        to_session: opt_str_field(&existing, "harness_session_id"),
    })
}

/// The lock-timeout grace path: re-acquire on a grace window, verify the
/// recipient, queue durable (exit 0 to the caller), and only sustained
/// contention exits 11 with nothing written.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_send_lock_timeout(
    requested_name: &str,
    canonical_name: &str,
    canonical_identity: &str,
    message: &str,
    provider: Option<&str>,
    from_name: &str,
    origin: Option<&str>,
    subject: Option<&str>,
    timeout_err: LockTimeout,
) -> Result<DispatchSendResult, (String, i32)> {
    let grace_seconds = queue_grace_seconds(timeout_err.timeout);
    let grace_on_wait = || {
        eprintln!("Waiting for agent {canonical_name:?} lock...");
    };
    let grace_lock = match AgentLock::acquire(canonical_name, grace_seconds, Some(&grace_on_wait)) {
        Ok(lock) => lock,
        Err(still_held) => {
            emit_event(
                "agent_send_failed",
                json!({"stage": "lock-timeout", "name": canonical_name, "reason": "unverified_recipient"}),
            );
            return Err((
                format!(
                    "timed out waiting for agent {:?} lock (timeout={}s + {grace_seconds}s queue grace){}; recipient identity could not be verified, so no durable envelope was written; retry the send",
                    timeout_err.name,
                    timeout_err.timeout,
                    still_held.holder_note(false)
                ),
                11,
            ));
        }
    };
    let run = dispatch_send_lock_timeout_locked(
        requested_name,
        canonical_name,
        canonical_identity,
        message,
        provider,
        from_name,
        origin,
        subject,
        &timeout_err,
        grace_lock,
    );
    run
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_send_lock_timeout_locked(
    requested_name: &str,
    canonical_name: &str,
    canonical_identity: &str,
    message: &str,
    provider: Option<&str>,
    from_name: &str,
    origin: Option<&str>,
    subject: Option<&str>,
    timeout_err: &LockTimeout,
    grace_lock: AgentLock,
) -> Result<DispatchSendResult, (String, i32)> {
    let (timeout_entries, timeout_entry) = load_and_resolve_target(requested_name)?;
    if recipient_identity(&timeout_entry) != canonical_identity {
        return Err((
            format!(
                "agent address {requested_name:?} changed while acquiring its lock; recipient identity changed; retry the send"
            ),
            2,
        ));
    }
    let _ = canonical_identity;
    select_provider(
        str_field(&timeout_entry, "name"),
        provider,
        opt_str_field(&timeout_entry, "harness").as_deref(),
    )?;
    if opt_str_field(&timeout_entry, "harness_session_id").is_none() {
        emit_event(
            "agent_send_failed",
            json!({"stage": "durable-address", "name": canonical_name, "reason": "missing_harness_session_id", "caller_reason": LOCK_TIMEOUT_REASON}),
        );
        return Err((
            format!(
                "live delivery to {canonical_name:?} was not attempted (its agent lock was busy for {}s, and this queue holds it now), and its registry row carries no full harness session id, so it has no durable address; {NO_ENVELOPE_CLAUSE}; retry the send",
                timeout_err.timeout
            ),
            11,
        ));
    }
    let queue_reason =
        if delivery_policy_refusal(opt_str_field(&timeout_entry, "name").as_deref(), None)
            == Some(BUS_ONLY_POLICY.to_string())
        {
            BUS_ONLY_POLICY.to_string()
        } else {
            LOCK_TIMEOUT_REASON.to_string()
        };
    let msg_id = generate_msg_id();
    let (from_harness, from_session) = stamped_sender_provenance(
        resolve_sender_entry(&timeout_entries, from_name).as_ref(),
        from_name,
    );
    let timeout_recipient = opt_str_field(&timeout_entry, "harness_session_id")
        .map(|sid| crate::identity::canonical_handle(&sid))
        .unwrap_or_default();
    let timeout_mail_ctx = build_mail_ctx(
        from_name,
        from_session.clone(),
        from_harness.clone(),
        Some(timeout_recipient.clone()),
        Some(msg_id.clone()),
        origin,
        opt_str_field(&timeout_entry, "harness_session_id"),
        subject,
    );
    let reservation = reserve_send_budget(
        &timeout_mail_ctx.from_,
        &timeout_recipient,
        message,
        &msg_id,
    );
    if let Err((msg, code)) = &reservation {
        return Err((msg.clone(), *code));
    }
    let reservation = reservation.expect("checked above");
    emit_event(
        "agent_send_started",
        json!({"name": canonical_name, "provider": str_field(&timeout_entry, "harness"), "msg_id": msg_id}),
    );
    let queued = queue_durable_fallback(
        &timeout_entry,
        message,
        &timeout_mail_ctx,
        &msg_id,
        "wake-daemon",
        subject,
    );
    let (msg_id, durable_to) = match queued {
        Ok(pair) => pair,
        Err((msg, code)) => {
            release_budget(&reservation);
            return Err((msg, code));
        }
    };
    emit_event(
        "agent_send_done",
        json!({"name": canonical_name, "provider": str_field(&timeout_entry, "harness"), "msg_id": msg_id, "delivery": "durable", "reason": queue_reason}),
    );
    stamp_after_delivery(canonical_name, "durable", canonical_identity);
    drop(grace_lock);
    if queue_reason == LOCK_TIMEOUT_REASON {
        eprintln!(
            "live delivery deferred for {canonical_name:?}: lock was busy for {}s{}",
            timeout_err.timeout,
            timeout_err.holder_note(true)
        );
    }
    Ok(DispatchSendResult {
        msg_id,
        delivery: "durable".to_string(),
        reason: Some(queue_reason),
        durable_owner: None,
        to: if durable_to.is_empty() {
            None
        } else {
            Some(durable_to)
        },
        to_harness: opt_str_field(&timeout_entry, "harness"),
        to_session: opt_str_field(&timeout_entry, "harness_session_id"),
    })
}

// ---------------------------------------------------------------------------
// Project anycast resolution (dispatch.py resolve_to_project twin)
// ---------------------------------------------------------------------------

/// AMBIGUOUS_PROJECT_EXIT_CODE
pub(super) fn project_ambiguity_error(project: &str, live_candidates: &[String]) -> (String, i32) {
    let listing = live_candidates.join(", ");
    (
        format!(
            "--to-project {project:?} is ambiguous: {} live peers ({listing}); pass --any to break the tie or address one by name. Delivered to none.",
            live_candidates.len()
        ),
        AMBIGUOUS_PROJECT_EXIT,
    )
}

/// Python `resolve_to_project`: exactly one live peer delivers live, none
/// queues durable to the project, many refuse unless --any breaks the tie.
pub(super) fn resolve_to_project(
    project: &str,
    any_flag: bool,
) -> Result<(Option<String>, Vec<String>, bool), (String, i32)> {
    // (recipient, live_candidates, durable)
    let registry = AgentsHome::shared_registry_json();
    let rows = match crate::client_verbs::load_registry_entries(&registry) {
        Ok(rows) => rows,
        Err(exc) => return Err((format!("registry read failed: {exc}"), 12)),
    };
    let mut live_names: Vec<String> = Vec::new();
    let mut live_rows: Vec<Value> = Vec::new();
    for row in rows.iter() {
        if str_field(row, "status") != "live" {
            continue;
        }
        if project_serves(row, project) {
            live_names.push(str_field(row, "name").to_string());
            live_rows.push(row.clone());
        }
    }
    live_names.sort();
    if live_names.is_empty() {
        return Ok((None, Vec::new(), true));
    }
    if live_names.len() == 1 {
        return Ok((Some(live_names[0].clone()), live_names, false));
    }
    if !any_flag {
        return Err(project_ambiguity_error(project, &live_names));
    }
    // --any tiebreak: most recent last_message_at, then lexicographic name.
    let mut best: Option<(String, String)> = None;
    for row in live_rows.iter() {
        let ts = opt_str_field(row, "last_message_at").unwrap_or_default();
        let name = str_field(row, "name").to_string();
        match &best {
            None => best = Some((ts, name)),
            Some((best_ts, _)) => {
                if ts > *best_ts || (ts == *best_ts && name < best.clone().unwrap().1) {
                    best = Some((ts, name));
                }
            }
        }
    }
    let winner = best.map(|(_, name)| name).unwrap_or_default();
    Ok((Some(winner), live_names, false))
}

/// Python `_entry_projects` + the membership test: the registry cwd mapping
/// resolves through the entry's own cwd; the config peer hint only adds.
pub(super) fn project_serves(row: &Value, project: &str) -> bool {
    let cwd = opt_str_field(row, "cwd");
    if let Some(cwd) = cwd {
        if let Some(pid) = project_id_from_cwd(&cwd) {
            if pid == project {
                return true;
            }
        }
    }
    false
}

/// Python `resolve_project`'s cwd walk: `.fno/config.toml` (or legacy
/// settings.yaml) names the project; unresolvable cwds contribute nothing.
pub(super) fn project_id_from_cwd(cwd: &str) -> Option<String> {
    let mut search = PathBuf::from(cwd);
    loop {
        let toml_path = search.join(".fno").join("config.toml");
        let yaml_path = search.join(".fno").join("settings.yaml");
        let candidate = if toml_path.is_file() {
            Some(toml_path)
        } else if yaml_path.is_file() {
            Some(yaml_path)
        } else {
            None
        };
        if let Some(path) = candidate {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Some(pid) = project_id_from_settings_text(&text) {
                    return Some(pid);
                }
            }
        }
        if !search.pop() {
            break;
        }
    }
    None
}

/// Python `_project_id_from_settings`: the `project` key, flat or nested.
pub(super) fn project_id_from_settings_text(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        let key = trimmed[..eq].trim().trim_matches('"');
        if key != "project" {
            continue;
        }
        let mut value = trimmed[eq + 1..].trim().to_string();
        if value.ends_with(',') {
            value.pop();
        }
        let value = value.trim_matches('"').trim_matches('\'').to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

//! `-H muse --substrate headless`: one `muse exec --json --session-id <id>`
//! turn. The first call with an id creates the session and a later call with
//! the same id continues it, so spawn and ask share one form.

use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{outcome, resolve_muse_bin};
use crate::claude_ask::AskOutcome;
use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry, RegistryEntry};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1800);

/// The turn's `run_terminal` record: `(terminal, text)`. Err names the last
/// record kind when the stream has no terminal record.
pub fn parse_exec(stdout: &str) -> Result<(String, String), String> {
    let mut last = String::from("none");
    for line in stdout.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &record["payload"];
        let kind = payload["kind"].as_str().unwrap_or("");
        if kind == "run_terminal" {
            let terminal = payload["terminal"].as_str().unwrap_or("").to_string();
            let text = payload["text"].as_str().unwrap_or("").to_string();
            return Ok((terminal, text));
        }
        if !kind.is_empty() {
            last = kind.to_string();
        }
    }
    Err(last)
}

/// The exec argv: the row's form, then the mapped mode, the fenced tokens,
/// and the message after `--`. The spawn door's `--permission-mode` rides
/// the fence; exec spells it `--approval-mode`.
pub fn exec_argv(
    bin: &str,
    session_id: &str,
    harness_args: &[String],
    permission_mode: Option<&str>,
    message: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = crate::harness_capabilities::render_session_argv(
        "muse",
        "headless_create",
        Some(session_id),
    )
    .unwrap_or_else(|_| {
        ["muse", "exec", "--json", "--session-id", session_id]
            .map(str::to_string)
            .to_vec()
    });
    argv[0] = bin.to_string();
    if let Some(mode) = permission_mode.filter(|_| {
        !harness_args
            .iter()
            .any(|a| a == "--approval-mode" || a == "--permission-mode")
    }) {
        argv.extend(["--approval-mode".to_string(), mode.to_string()]);
    }
    argv.extend(harness_args.iter().map(|a| match a.as_str() {
        "--permission-mode" => "--approval-mode".to_string(),
        _ => a.clone(),
    }));
    argv.extend(["--".to_string(), message.to_string()]);
    argv
}

/// Run one exec turn, its stdout teed to `log`. Returns the exit code (None
/// when it timed out) and the stdout.
fn run_exec(
    argv: &[String],
    cwd: &Path,
    name: &str,
    sid: &str,
    log: &Path,
    timeout: Duration,
) -> Result<(Option<i32>, String, String), String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .process_group(0);
    crate::claims::stamp_command_env(&mut cmd, Some(name), "muse", Some(sid));
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start muse exec: {e}"))?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                // SAFETY: the child leads its own group (process_group(0)).
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    // Every turn of the session appends to one log.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log);
    if let Ok(mut f) = file {
        use std::io::Write;
        let _ = f.write_all(stdout.as_bytes());
    }
    Ok((code, stdout, stderr))
}

/// The receipt for one finished exec. Exit 0 only for a `completed`
/// terminal from a process that exited 0.
fn receipt(
    name: &str,
    sid: &str,
    log: &Path,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> AskOutcome {
    let parsed = parse_exec(stdout);
    let ok = code == Some(0) && matches!(&parsed, Ok((t, _)) if t == "completed");
    let mut r = json!({"ok": ok, "name": name, "session_id": sid, "substrate": "headless", "log_path": log, "exit_code": code});
    match parsed {
        Ok((terminal, text)) => {
            r["terminal"] = json!(terminal);
            r["text"] = json!(text);
        }
        Err(last) => {
            r["terminal"] = json!("failed");
            r["reason"] = json!(format!("no run_terminal record; last record kind {last}"));
        }
    }
    let tail = if ok {
        String::new()
    } else {
        stderr.to_string()
    };
    outcome(if ok { 0 } else { 2 }, format!("{r}\n"), tail)
}

fn timeout_of(timeout: Option<Duration>) -> Duration {
    timeout.unwrap_or(DEFAULT_TIMEOUT)
}

fn log_path(home: &AgentsHome, sid: &str) -> PathBuf {
    home.root().join("muse").join(format!("{sid}.exec.jsonl"))
}

/// `fno agents spawn -H muse --once`: mint the id, run the turn, register
/// the row Exited with the session id so `ask` can continue it.
pub fn dispatch_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    params: &Value,
    timeout: Option<Duration>,
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
    let bin = match resolve_muse_bin() {
        Ok(b) => b,
        Err(e) => return outcome(2, String::new(), format!("{e}\n")),
    };
    let sid = super::msp::mint_uuid_v7();
    let mode = params["permission_mode"].as_str();
    let args = super::fenced_args(params);
    let prompt = if message.is_empty() { "hello" } else { message };
    let argv = exec_argv(&bin.to_string_lossy(), &sid, &args, mode, prompt);
    let log = log_path(home, &sid);
    let _ = std::fs::create_dir_all(log.parent().unwrap_or(Path::new(".")));
    let (code, stdout, stderr) = match run_exec(&argv, cwd, name, &sid, &log, timeout_of(timeout)) {
        Ok(r) => r,
        Err(e) => return outcome(13, String::new(), format!("{e}\n")),
    };
    let entry = RegistryEntry {
        name: name.to_string(),
        short_id: sid.clone(),
        provider: Some("muse".into()),
        harness: Some("muse".into()),
        substrate: Some("headless".into()),
        session_id: Some(sid.clone()),
        harness_session_id: Some(sid.clone()),
        requested_permission_mode: mode.map(str::to_string),
        node: params["node"].as_str().map(str::to_string),
        cwd: cwd.to_string_lossy().to_string(),
        origin: Some("spawn".into()),
        status: crate::AgentStatus::Exited,
        created_at: crate::daemon::now_rfc3339_like(),
        log_path: Some(log.to_string_lossy().to_string()),
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
    if let Err(e) = registered {
        return outcome(12, String::new(), format!("registry write failed: {e}\n"));
    }
    receipt(name, &sid, &log, code, &stdout, &stderr)
}

/// `fno agents ask <name>` on a muse row with no live host: one more exec
/// turn on the row's session id.
pub fn resume(home: &AgentsHome, entry: &RegistryEntry, params: &Value) -> i32 {
    let Some(sid) = entry.harness_session_id.clone() else {
        eprintln!("fno-agents: muse row {} names no session id", entry.name);
        return 2;
    };
    let bin = match resolve_muse_bin() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fno-agents: {e}");
            return 2;
        }
    };
    let message = params["message"].as_str().unwrap_or("");
    if message.is_empty() {
        eprintln!("fno-agents: ask on a muse row needs a message");
        return 2;
    }
    let mode = entry.requested_permission_mode.as_deref();
    let argv = exec_argv(&bin.to_string_lossy(), &sid, &[], mode, message);
    let log = log_path(home, &sid);
    let timeout = params["timeout"].as_u64().map(Duration::from_secs);
    let cwd = PathBuf::from(&entry.cwd);
    let o = match run_exec(&argv, &cwd, &entry.name, &sid, &log, timeout_of(timeout)) {
        Ok((code, stdout, stderr)) => receipt(&entry.name, &sid, &log, code, &stdout, &stderr),
        Err(e) => outcome(13, String::new(), format!("{e}\n")),
    };
    print!("{}", o.stdout);
    eprint!("{}", o.stderr);
    o.exit_code
}

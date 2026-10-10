//! `-H footnote` on the supervisor side. The loop itself is the `footnote`
//! binary, built from bllshttng/fnh; this module resolves everything it needs, hands
//! it one `LaunchSpec` on stdin, and keeps the registry row, the way
//! `zcode_ask` launches zcode.

pub mod endpoint;
pub mod source;
pub mod transcript;

use serde_json::Value;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::claude_ask::AskOutcome;
use crate::footnote_transcript::{LaunchSpec, LAUNCH_SPEC_V};
use crate::paths::AgentsHome;
use crate::state::{load_registry, update_registry, RegistryEntry};

const INSTALL_HINT: &str =
    "build it from bllshttng/fnh: cargo install --locked --path ~/code/footnote/fnh";

/// The `footnote` binary: `FNO_FOOTNOTE_BIN`, else a sibling of this
/// executable, else `footnote` on PATH.
pub fn resolve_footnote_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_FOOTNOTE_BIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("footnote")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("footnote"))
}

/// Budget caps and the claimed plan from the target manifest, when one exists.
fn manifest_fields(
    manifest: Option<&Path>,
) -> (Option<f64>, Option<u64>, Option<String>, Option<String>) {
    let Some(content) = manifest.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return (None, None, None, None);
    };
    let f = |k: &str| crate::loopcheck::scan_manifest_field(&content, k);
    (
        f("budget_cost_cap_usd")
            .and_then(|v| v.parse().ok())
            .filter(|v: &f64| *v > 0.0),
        f("budget_wall_clock_cap_minutes")
            .and_then(|v| v.parse().ok())
            .filter(|v: &u64| *v > 0),
        f("plan_path"),
        f("graph_node_id"),
    )
}

/// The models.dev price cache beside the sessions root.
fn price_cache() -> PathBuf {
    transcript::sessions_root()
        .parent()
        .map(|p| p.join("cache").join("models-dev.json"))
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn launch_spec(
    mode: &str,
    fno_id: &str,
    session_dir: PathBuf,
    cwd: &Path,
    model: &str,
    message: &str,
    timeout: Option<Duration>,
    node: Option<&str>,
    permission_mode: Option<&str>,
) -> Result<LaunchSpec, String> {
    let endpoint = endpoint::resolve_endpoint(cwd, &|k| std::env::var(k).ok())?;
    let manifest = crate::state_path::resolve("target-state", cwd);
    let (cost_cap_usd, wall_cap_minutes, plan_path, manifest_node) =
        manifest_fields(manifest.as_deref());
    Ok(LaunchSpec {
        v: LAUNCH_SPEC_V,
        mode: mode.to_string(),
        fno_id: fno_id.to_string(),
        session_dir,
        cwd: cwd.to_path_buf(),
        model: model.to_string(),
        message: message.to_string(),
        timeout_secs: timeout.map(|d| d.as_secs()),
        node: node.map(str::to_string).or(manifest_node),
        plan_path,
        cost_cap_usd,
        wall_cap_minutes,
        plugin_root: crate::provider::plugin_root(),
        parent_session_id: std::env::var("FNO_HARNESS_SESSION_ID")
            .ok()
            .filter(|v| !v.is_empty()),
        permission_mode: permission_mode.map(str::to_string),
        price_cache: price_cache(),
        context_window: crate::context_window::window_for_model(model),
        endpoint,
    })
}

/// Start the binary in its own process group with the spawner's identity
/// scrubbed, so its tool children carry only footnote's own.
fn spawn(cwd: &Path) -> Result<Child, AskOutcome> {
    let bin = resolve_footnote_bin();
    let mut cmd = Command::new(&bin);
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in crate::claims::AMBIENT_IDENTITY_NAMES {
        cmd.env_remove(name);
    }
    // SAFETY: setpgid is async-signal-safe; it runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| {
        let why = if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "footnote binary not found at {}; {INSTALL_HINT}",
                bin.display()
            )
        } else {
            format!("cannot start {}: {e}", bin.display())
        };
        outcome(2, String::new(), format!("{why}\n"))
    })
}

/// Feed the spec and wait. Ctrl-C reaches the child's group, which records
/// the interrupt in its transcript before it exits.
fn finish(mut child: Child, spec: &LaunchSpec) -> AskOutcome {
    let _sigint = crate::subprocess_ask::SigintForwarder::install(child.id());
    let body = serde_json::to_vec(spec).unwrap_or_default();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&body);
    }
    match child.wait_with_output() {
        Ok(out) => outcome(
            out.status
                .code()
                .unwrap_or(if crate::subprocess_ask::ask_interrupted() {
                    130
                } else {
                    12
                }),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(e) => outcome(12, String::new(), format!("footnote: wait failed: {e}\n")),
    }
}

fn outcome(code: i32, stdout: String, stderr: String) -> AskOutcome {
    AskOutcome {
        stdout,
        stderr,
        exit_code: code,
    }
}

fn mark_row(home: &AgentsHome, name: &str, transcript: &Path) {
    let reported = transcript::read_records(transcript)
        .ok()
        .and_then(|records| {
            records
                .iter()
                .find(|r| r["type"] == "model_response")
                .and_then(|r| r["data"]["reported_model"].as_str())
                .map(str::to_string)
        });
    let _ = update_registry(&home.registry_json(), |reg| {
        let Some(e) = reg.find_mut(name) else {
            return false;
        };
        e.status = crate::AgentStatus::Exited;
        e.pid = None;
        e.last_message_at = Some(crate::daemon::now_rfc3339_like());
        if e.model_name.is_none() {
            e.model_name = reported.clone();
        }
        true
    });
}

/// `fno agents spawn -H footnote --substrate headless`: mint the id, start
/// the binary, register the row with the child's pid, run, mark it Exited.
/// `params` is the spawn request; it supplies `node` and `permission_mode`.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_once(
    home: &AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    model: Option<&str>,
    timeout: Option<Duration>,
    params: &Value,
) -> AskOutcome {
    if let Err(msg) = crate::claude_ask::validate_spawn_inputs(name, from_name) {
        return outcome(2, String::new(), format!("{msg}\n"));
    }
    let Some(model) = model.filter(|m| !m.is_empty()) else {
        return outcome(2, String::new(), "-H footnote needs -m <model>\n".into());
    };
    match load_registry(&home.registry_json()) {
        Ok(reg) if reg.find(name).is_some() => {
            return outcome(
                2,
                String::new(),
                format!("agent {name} already exists; use 'fno agents rm {name}' first\n"),
            );
        }
        Err(e) => return outcome(12, String::new(), format!("registry read failed: {e}\n")),
        Ok(_) => {}
    }
    let sid = match crate::identity::mint_fno_id() {
        Ok(id) => id,
        Err(e) => return outcome(2, String::new(), format!("{e}\n")),
    };
    let dir = transcript::session_dir(&transcript::sessions_root(), cwd, &sid);
    let record = transcript::transcript_file(&dir, &sid);
    let node = params.get("node").and_then(Value::as_str);
    let mode = params.get("permission_mode").and_then(Value::as_str);
    let spec = match launch_spec(
        "create", &sid, dir, cwd, model, message, timeout, node, mode,
    ) {
        Ok(s) => s,
        Err(e) => return outcome(2, String::new(), format!("{e}\n")),
    };
    let mut child = match spawn(cwd) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let mut entry = RegistryEntry {
        name: name.to_string(),
        short_id: sid.clone(),
        provider: Some("footnote".into()),
        harness: Some("footnote".into()),
        substrate: Some("headless".into()),
        session_id: Some(sid.clone()),
        requested_model: Some(model.to_string()),
        requested_permission_mode: spec.permission_mode.clone(),
        route_provider_id: spec.endpoint.provider_id.clone(),
        node: spec.node.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        origin: Some("spawn".into()),
        status: crate::AgentStatus::Busy,
        pid: Some(child.id()),
        created_at: crate::daemon::now_rfc3339_like(),
        log_path: Some(record.to_string_lossy().to_string()),
        ..RegistryEntry::new(Some(sid.clone()), crate::spawn_lineage::ambient_lineage())
    };
    entry.account_record_id = Some("default".into());
    // The session's own id is the row's fno_id; a set value is never re-minted.
    entry.fno_id = Some(sid.clone());
    let registered = update_registry(&home.registry_json(), |reg| {
        if reg.find(name).is_some() {
            return false;
        }
        reg.entries.push(entry.clone());
        true
    });
    if !matches!(registered, Ok(true)) {
        // The child is still blocked on stdin: close it and reap it.
        let _ = child.kill();
        let _ = child.wait();
        return match registered {
            Ok(_) => outcome(2, String::new(), format!("agent {name} already exists\n")),
            Err(e) => outcome(12, String::new(), format!("registry write failed: {e}\n")),
        };
    }
    let out = finish(child, &spec);
    if out.exit_code == 2 && !record.is_file() {
        // Refused before the session existed: leave no row, as before.
        let _ = update_registry(&home.registry_json(), |reg| {
            let before = reg.entries.len();
            reg.entries.retain(|e| e.name != name);
            reg.entries.len() != before
        });
    } else {
        mark_row(home, name, &record);
    }
    out
}

/// `fno agents ask <name>` on a footnote row: resume by name, then the input.
/// Returns None for any other harness (fall through).
pub fn maybe_run_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    let reg = load_registry(&home.registry_json()).ok()?;
    let entry = reg.find_name_or_full_session_id(name)?;
    if entry.harness_name() != "footnote" {
        return None;
    }
    let fno_id = entry
        .fno_id
        .clone()
        .or_else(|| entry.harness_session_id.clone())?;
    let row_name = entry.name.clone();
    let cwd = PathBuf::from(&entry.cwd);
    let model = entry.requested_model.clone().unwrap_or_default();
    // A resume keeps the mode the session was spawned with.
    let mode = entry.requested_permission_mode.clone();
    let message = params["message"].as_str().unwrap_or("").to_string();
    let timeout = params["timeout"].as_u64().map(Duration::from_secs);
    let Some(dir) = transcript::find_session_dir(&transcript::sessions_root(), &fno_id) else {
        eprintln!("fno-agents: footnote session {fno_id} has no transcript on disk");
        return Some(2);
    };
    let record = transcript::transcript_file(&dir, &fno_id);
    let spec = match launch_spec(
        "resume",
        &fno_id,
        dir,
        &cwd,
        &model,
        &message,
        timeout,
        None,
        mode.as_deref(),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fno-agents: {e}");
            return Some(2);
        }
    };
    let child = match spawn(&cwd) {
        Ok(c) => c,
        Err(o) => {
            eprint!("{}", o.stderr);
            return Some(o.exit_code);
        }
    };
    let pid = child.id();
    let prior = (entry.status, entry.pid);
    let _ = update_registry(&home.registry_json(), |reg| {
        let Some(e) = reg.find_mut(&row_name) else {
            return false;
        };
        e.status = crate::AgentStatus::Busy;
        e.pid = Some(pid);
        true
    });
    let o = finish(child, &spec);
    if o.exit_code == 2 {
        // Refused before the session reopened (a live writer holds it, or
        // the spec was refused): the row goes back to what it said, so a
        // running session is never marked Exited by a second ask.
        let _ = update_registry(&home.registry_json(), |reg| {
            let Some(e) = reg.find_mut(&row_name) else {
                return false;
            };
            (e.status, e.pid) = prior;
            true
        });
    } else {
        mark_row(home, &row_name, &record);
    }
    print!("{}", o.stdout);
    eprint!("{}", o.stderr);
    Some(o.exit_code)
}

/// The roster's `footnote` row. Spawn and ask never reach these argv
/// methods: the client launches the binary itself (`dispatch_once`,
/// `maybe_run_ask`), so they render the capability table's forms only.
pub struct FootnoteProvider;

impl crate::provider::Provider for FootnoteProvider {
    fn name(&self) -> &'static str {
        "footnote"
    }

    fn create_argv(&self, _ctx: &crate::provider::CreateContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv("footnote", "headless_create", None)
            .expect("embedded footnote headless-create capability")
    }

    fn resume_argv(&self, ctx: &crate::provider::ResumeContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv(
            "footnote",
            "headless_resume",
            Some(&ctx.session_id),
        )
        .expect("embedded footnote headless-resume capability")
    }

    fn parse_stream_event(&self, chunk: &str) -> crate::ParsedEvent {
        crate::ParsedEvent::Unknown {
            raw: chunk.to_string(),
        }
    }

    fn reachability(
        &self,
        _entry: &crate::provider::AgentEntry,
        _timeout: Duration,
    ) -> Result<bool, crate::provider::ReachabilityProbeError> {
        Err(crate::provider::ReachabilityProbeError::new(
            "footnote",
            "each turn is a one-shot footnote child; read the transcript's terminal record",
        ))
    }
}

#[cfg(test)]
mod tests;

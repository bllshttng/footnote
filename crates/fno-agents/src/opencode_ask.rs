//! Client interceptor for the `ask` verb on an opencode target (x-51f6), plus
//! the headless one-shot dispatch (`dispatch_opencode_once`).
//!
//! opencode is hosted two ways: an interactive PTY pane (the `ask` resume path,
//! which stays refused — no stateful client-side resume in v1) and a headless
//! one-shot `opencode run --dangerously-skip-permissions "<prompt>"` (this module's `dispatch_opencode_once`,
//! substrate `headless`). The one-shot is STATELESS like agy: plain-text stdout,
//! no session id minted here, no registry row, no `--continue` resume from this
//! path. It reuses the shared `subprocess_ask` primitives (stdin `/dev/null`,
//! watchdog, process-group SIGINT) rather than duplicating agy's error taxonomy.
//!
//! `ask` (resume-by-name) still refuses: it names the real limitation instead of
//! falling through to `bin/client.rs`'s `unresolvable_ask_exit` "provider is
//! required for new agent" text (wrong — the agent exists — and a dead end).
//! Mirrors [`crate::agy_ask::maybe_run_agy_ask`]'s shape.

/// Returns `None` for a non-opencode target (fall through to the next
/// provider's ask hook), or `Some(2)` after printing the refusal.
pub fn maybe_run_opencode_ask(
    home: &crate::paths::AgentsHome,
    params: &serde_json::Value,
    name: &str,
) -> Option<i32> {
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let registry = match crate::state::load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "fno-agents: cannot read agents registry at {:?}: {}",
                home.registry_json(),
                e
            );
            return Some(12);
        }
    };
    let existing_provider = registry.find(name).map(|e| e.provider.as_str());
    let resolved = existing_provider.or(provider_param);
    if resolved != Some("opencode") {
        return None; // not an opencode target; fall through
    }
    eprintln!(
        "fno-agents: opencode has no stateful 'ask' resume (pane-hosted, no client-side dispatch); \
         drive the pane directly with 'fno mux pane send <pane> --session <session> --text <prompt>'."
    );
    Some(2)
}

// ===========================================================================
// Headless one-shot dispatch (`opencode run`) — stateless, plain-text
// ===========================================================================

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default one-shot timeout; the outer watchdog bounds a headless `opencode run`
/// so a hang can't wedge the caller (a full /target run is long, so this is
/// generous). Caller may override via `timeout`.
const DEFAULT_OPENCODE_TIMEOUT: Duration = Duration::from_secs(600);

/// Stdout/stderr/exit triple returned to the client (mirror of the sibling
/// provider `AskOutcome`s; each module owns its own nominal type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl AskOutcome {
    fn ok_reply(reply: String) -> Self {
        Self {
            stdout: reply,
            stderr: String::new(),
            exit_code: 0,
        }
    }
    fn err(msg: impl Into<String>, code: i32) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("{}\n", msg.into()),
            exit_code: code,
        }
    }
}

/// `opencode run --dangerously-skip-permissions [--model <m>] <tail>` — the
/// headless one-shot argv (matches `OpencodeProvider::create_argv`). The bypass
/// flag auto-approves permissions so an unattended worker never wedges on an
/// approval prompt; confirmed against opencode v1.14.50's `run --help` (the
/// docs' `--auto` is stale — the shipped binary renamed it). The trailing argv
/// is built by `opencode_run_tail`: a footnote slash command rides `--command`
/// (opencode expands the plugin command), a prose prompt stays the message
/// positional (x-de43 / codex P1).
fn build_opencode_argv(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "opencode".to_string(),
        "run".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    if let Some(m) = model {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    argv.extend(crate::provider::opencode_run_tail(prompt));
    argv
}

/// Last `n` characters of `s` (UTF-8 safe; forensic tail for a failed run).
/// Walks from the end for the start byte offset instead of collecting every
/// char into a Vec, so a large stderr blob costs O(n), not O(len).
fn tail_chars(s: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    match s.char_indices().rev().nth(n - 1) {
        Some((idx, _)) => &s[idx..],
        None => s, // fewer than n chars
    }
}

/// Stable per-agent log path (mirror of the sibling one-shots).
fn derive_log_path(home: &crate::paths::AgentsHome, name: &str) -> std::path::PathBuf {
    home.root()
        .join("agents")
        .join("logs")
        .join(format!("{}.jsonl", name))
}

/// Drive one `opencode run` subprocess: stdin `/dev/null` (never block on a
/// non-TTS input wait), stdout captured as plain text, stderr drained on a
/// thread (bounded pipe), the whole call bounded by the shared watchdog. Lean
/// mirror of `agy_ask::run_agy` — same shape, no elaborate stderr taxonomy: a
/// non-zero exit surfaces the stderr tail, a hang maps to the timeout code.
///
/// Returns the plain-text reply on a clean exit, or `(exit_code, message)`.
fn run_opencode(
    argv: &[String],
    output_path: &Path,
    timeout: Option<Duration>,
    cwd: &Path,
    agent_self: &str,
) -> Result<String, (i32, String)> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let tee = crate::subprocess_ask::open_tee(output_path).ok();
    // QoS: exec-wrap at background priority (identity when worker_qos=off).
    let argv = crate::spawn_gate::qos_wrap(cwd, argv.to_vec());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.current_dir(cwd);
    cmd.env("FNO_AGENT_SELF", agent_self);
    cmd.env("FNO_AGENT_PROVIDER", "opencode");
    // Own process group so SIGTERM/SIGKILL/SIGINT reach opencode's subshells.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err((13, "opencode binary not found on PATH".to_string()));
        }
        Err(e) => return Err((2, format!("OSError invoking opencode: {}", e))),
    };
    let pid = child.id();
    // Forward operator Ctrl-C to opencode's process group for this call. RAII
    // guard held for the whole function — do NOT discard to `_`.
    let _sigint_guard = crate::subprocess_ask::SigintForwarder::install(pid);

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    // Drain stderr on a thread so a chatty stream can't deadlock the pipe while
    // we block on stdout; captured (unbounded is fine — opencode is not agy-loopy,
    // and a real runaway is bounded by the watchdog killing the process).
    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let cap = stderr_buf.clone();
    let stderr_handle = std::thread::spawn(move || {
        let mut r = stderr_pipe;
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        if let Ok(mut g) = cap.lock() {
            *g = s;
        }
    });

    let mut watchdog = crate::subprocess_ask::AskWatchdog::spawn(pid, timeout);
    let mut stdout_bytes: Vec<u8> = Vec::new();
    let _ = stdout_pipe.read_to_end(&mut stdout_bytes);
    let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();
    if let Some(mut fh) = tee {
        let _ = fh.write_all(stdout_text.as_bytes());
    }

    watchdog.cancel();
    let (exit_code, sigkill_escalated) =
        crate::subprocess_ask::wait_with_grace(pid, &mut child, 5.0);
    watchdog.join();
    let _ = stderr_handle.join();
    let stderr_text = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();

    // Operator Ctrl-C wins over every other classification.
    if crate::subprocess_ask::ask_interrupted() {
        return Err((130, "interrupted".to_string()));
    }
    if watchdog.timed_out() || sigkill_escalated {
        return Err((
            12,
            format!(
                "opencode run timed out after {:.0}s",
                timeout.unwrap_or(DEFAULT_OPENCODE_TIMEOUT).as_secs_f64()
            ),
        ));
    }
    if exit_code != 0 {
        return Err((
            exit_code,
            format!(
                "opencode run exited {}: {}",
                exit_code,
                tail_chars(stderr_text.trim(), 400)
            ),
        ));
    }
    Ok(stdout_text)
}

/// Orchestrate one opencode `spawn --substrate headless`: validate, fail-closed
/// registry + name-collision check, then run `opencode run` and return the
/// reply. STATELESS by design — opencode's own `--session`/`--continue` resume is
/// a pane/interactive concern, not this one-shot; `name` labels the log + events.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_opencode_once(
    home: &crate::paths::AgentsHome,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    _yolo: bool, // opencode uses --auto for permission bypass; yolo is a no-op (agy parity)
    timeout: Option<Duration>,
    model: Option<&str>,
) -> AskOutcome {
    use crate::claude_ask::{emit_event, py_repr, validate_spawn_inputs};

    if let Err(msg) = validate_spawn_inputs(name, from_name) {
        return AskOutcome::err(msg, 2);
    }
    let events = home.events_jsonl();

    // Authoritative registry read, fail-closed (the caller's collision check is
    // advisory `unwrap_or_default`); re-check the name collision under this read.
    let registry = match crate::state::load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "registry-read".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", e.to_string().into()),
                ],
            );
            return AskOutcome::err(format!("registry read failed: {}", e), 12);
        }
    };
    if registry.find(name).is_some() {
        return AskOutcome::err(
            format!(
                "agent {} already exists; use 'fno agents rm {}' first or pick another name",
                py_repr(name),
                name
            ),
            2,
        );
    }

    // spawn allows an empty initial message; default to "hello" (Python parity —
    // only the empty string, not whitespace).
    let effective_message = if message.is_empty() { "hello" } else { message };
    // A footnote slash command (`/fno:verb ...`) is a command dispatch, not a
    // conversational message: send it WITHOUT the `[from:]` envelope so it rides
    // `opencode run --command` (an envelope would demote it to prose no-op). A
    // prose message keeps the courtesy envelope (x-de43).
    let full_prompt = if effective_message.starts_with('/') {
        effective_message.to_string()
    } else {
        format!("[from: {}]\n\n{}", from_name, effective_message)
    };
    let argv = build_opencode_argv(&full_prompt, model);
    let log_path = derive_log_path(home, name);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let eff_timeout = timeout.or(Some(DEFAULT_OPENCODE_TIMEOUT));

    match run_opencode(&argv, &log_path, eff_timeout, cwd, name) {
        Ok(reply) => AskOutcome::ok_reply(reply),
        Err((code, msg)) => {
            emit_event(
                &events,
                "agent_ask_failed",
                &[
                    ("stage", "opencode-once".into()),
                    ("name", name.into()),
                    ("provider", "opencode".into()),
                    ("error", msg.clone().into()),
                ],
            );
            AskOutcome::err(msg, code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(dir: &std::path::Path) -> crate::paths::AgentsHome {
        crate::paths::AgentsHome::at(dir.to_path_buf())
    }

    // Raw JSON (not a typed RegistryEntry) so this test only names the fields
    // it cares about; every other field has a `#[serde(default)]` on the real
    // struct and deserializes fine without them.
    fn write_registry_row(home: &crate::paths::AgentsHome, name: &str, provider: &str) {
        std::fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": [{
                "name": name,
                "provider": provider,
                "cwd": "/x",
                "status": "live",
                "created_at": "2026-01-01T00:00:00Z",
            }],
        });
        std::fs::write(home.registry_json(), body.to_string()).unwrap();
    }

    fn write_empty_registry(home: &crate::paths::AgentsHome) {
        std::fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": [],
        });
        std::fs::write(home.registry_json(), body.to_string()).unwrap();
    }

    #[test]
    fn non_opencode_target_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        write_empty_registry(&h);
        let params = serde_json::json!({"provider": "codex"});
        assert_eq!(maybe_run_opencode_ask(&h, &params, "wk"), None);
    }

    #[test]
    fn existing_opencode_row_refuses_by_registry_lookup_alone() {
        // No --provider flag needed: the registry lookup resolves it, exactly
        // like the "agent already exists" case the finding named.
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        write_registry_row(&h, "oc", "opencode");
        let params = serde_json::json!({});
        assert_eq!(maybe_run_opencode_ask(&h, &params, "oc"), Some(2));
    }

    #[test]
    fn provider_flag_alone_also_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        write_empty_registry(&h);
        let params = serde_json::json!({"provider": "opencode"});
        assert_eq!(maybe_run_opencode_ask(&h, &params, "new-oc"), Some(2));
    }

    #[test]
    fn argv_is_headless_run_bypass_with_prompt_last() {
        // Matches OpencodeProvider::create_argv (confirmed vs opencode v1.14.50).
        assert_eq!(
            build_opencode_argv("do X", None),
            vec!["opencode", "run", "--dangerously-skip-permissions", "do X"]
        );
    }

    #[test]
    fn argv_routes_footnote_slash_command_via_command_flag() {
        // A rendered `/fno:verb` rides `--command` so opencode expands the plugin
        // command instead of running it as prose (x-de43 / codex P1).
        assert_eq!(
            build_opencode_argv("/fno:target no-merge x-abcd", None),
            vec![
                "opencode",
                "run",
                "--dangerously-skip-permissions",
                "--command",
                "fno:target",
                "no-merge x-abcd"
            ]
        );
    }

    #[test]
    fn argv_threads_model_before_prompt() {
        assert_eq!(
            build_opencode_argv("m", Some("anthropic/claude-x")),
            vec![
                "opencode",
                "run",
                "--dangerously-skip-permissions",
                "--model",
                "anthropic/claude-x",
                "m"
            ]
        );
    }

    #[test]
    fn tail_chars_is_utf8_safe_and_bounded() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 5), "ab"); // fewer than n -> whole string
        assert_eq!(tail_chars("héllo", 3), "llo"); // never splits a codepoint
    }
}

//! `-H muse`: Muse Code. A thread is one `muse serve` session held over MSP
//! by an fno-owned host (`fno-agents-worker --muse-host`, host.rs); headless
//! is one `muse exec --json` turn (once.rs). Neither lane uses a PTY.

pub mod host;
pub mod msp;
pub mod once;
pub mod thread;

use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use crate::claude_ask::AskOutcome;
use crate::paths::AgentsHome;
use crate::state::load_registry;

/// The `muse` binary: `FNO_MUSE_BIN`, else `muse` found on PATH.
pub fn resolve_muse_bin() -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os("FNO_MUSE_BIN").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("muse"))
                .find(|b| b.is_file())
        })
        .ok_or_else(|| {
            "muse binary not found on PATH; install Muse Code, then `muse login`".to_string()
        })
}

fn outcome(code: i32, stdout: String, stderr: String) -> AskOutcome {
    AskOutcome {
        stdout,
        stderr,
        exit_code: code,
    }
}

/// The tokens a spawn passed after the `--` fence.
fn fenced_args(params: &Value) -> Vec<String> {
    params["harness_args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `fno agents spawn -H muse`: the thread host for `thread` (`bg`), one exec
/// turn for `headless`.
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    home: &AgentsHome,
    substrate: &str,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &std::path::Path,
    timeout: Option<Duration>,
    params: &Value,
) -> AskOutcome {
    if substrate == "headless" {
        once::dispatch_once(home, name, message, from_name, cwd, params, timeout)
    } else {
        thread::dispatch_thread(home, name, message, from_name, cwd, params)
    }
}

/// `fno agents ask <name>` on a muse row: the live host when one answers,
/// else one exec turn on the row's session. None for any other harness.
pub fn maybe_run_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    let reg = load_registry(&home.registry_json()).ok()?;
    let entry = reg.find_name_or_full_session_id(name)?;
    if entry.harness_name() != "muse" {
        return None;
    }
    if let Some(code) = thread::maybe_ask_host(entry, params) {
        return Some(code);
    }
    Some(once::resume(home, entry, params))
}

/// The roster's `muse` row. Spawn and ask never reach these argv methods:
/// the client runs the lanes itself, so they render the capability forms.
pub struct MuseProvider;

impl crate::provider::Provider for MuseProvider {
    fn name(&self) -> &'static str {
        "muse"
    }

    fn create_argv(&self, ctx: &crate::provider::CreateContext) -> Vec<String> {
        let id = ctx.session_id.clone().unwrap_or_else(msp::mint_uuid_v7);
        crate::harness_capabilities::render_session_argv("muse", "headless_create", Some(&id))
            .expect("embedded muse headless-create capability")
    }

    fn resume_argv(&self, ctx: &crate::provider::ResumeContext) -> Vec<String> {
        crate::harness_capabilities::render_session_argv(
            "muse",
            "headless_resume",
            Some(&ctx.session_id),
        )
        .expect("embedded muse headless-resume capability")
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
            "muse",
            "a thread row answers on its host socket; a headless row is one exec turn",
        ))
    }
}

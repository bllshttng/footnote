//! The one seam that names the stdio-host harnesses: footnote (ACP) and muse
//! (MSP). Each holds its session in an fno-owned host process with a unix
//! socket, so spawn, ask, mail and stop route here by harness and the
//! client and daemon only call these four functions.

use serde_json::Value;
use std::path::Path;
use std::time::Duration;

use crate::claude_ask::AskOutcome;
use crate::footnote_harness as footnote;
use crate::paths::AgentsHome;
use crate::protocol::{Request, Response};
use crate::state::RegistryEntry;

/// `fno agents spawn -H footnote|muse --substrate headless|bg`.
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    home: &AgentsHome,
    harness: &str,
    substrate: &str,
    name: &str,
    message: &str,
    from_name: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    params: &Value,
) -> AskOutcome {
    if harness == "muse" {
        return crate::muse::dispatch(
            home, substrate, name, message, from_name, cwd, timeout, params,
        );
    }
    let model = footnote::fenced_model(params);
    if substrate == "headless" {
        // footnote's own loop, run in this process.
        footnote::dispatch_once(
            home,
            name,
            message,
            from_name,
            cwd,
            model.as_deref(),
            timeout,
            params,
        )
    } else {
        // An fno-owned host holds fnh's ACP session open.
        footnote::thread::dispatch_thread(
            home,
            name,
            message,
            from_name,
            cwd,
            model.as_deref(),
            params,
        )
    }
}

/// `fno agents ask` on a footnote or muse row; None for any other harness.
pub fn maybe_run_ask(home: &AgentsHome, params: &Value, name: &str) -> Option<i32> {
    footnote::maybe_run_ask(home, params, name)
        .or_else(|| crate::muse::maybe_run_ask(home, params, name))
}

/// `agent.deliver`: live mail for a footnote or muse thread row.
pub fn deliver_rpc(home: &AgentsHome, req: &Request) -> Response {
    crate::muse::thread::deliver_rpc(home, req)
        .unwrap_or_else(|| footnote::thread::deliver_rpc(home, req))
}

/// Stop a footnote or muse row through its host; None for any other row.
pub(crate) async fn stop_row(entry: &RegistryEntry) -> Option<bool> {
    let pid = entry.pid?;
    match entry.harness_name() {
        "footnote" => Some(footnote::thread::stop_row(pid, entry).await),
        "muse" => Some(crate::muse::thread::stop_row(pid, entry).await),
        _ => None,
    }
}

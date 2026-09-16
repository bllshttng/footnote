//! The backend dispatch table: ONE router from a spawn
//! request's (harness, substrate) to the lane that executes it, resolved
//! through the existing capability contract. Every supported native detached,
//! server and headless combination resolves here or refuses by name; no lane
//! keeps a private routing decision.

use crate::harness_capabilities::HarnessContract;

/// The backend a spawn request resolves to. Variants name the lane; `Refused`
/// carries the same actionable refusal the lane used to emit inline, so an
/// unsupported combination stays honestly unsupported (AC8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedBackend {
    /// Codex app-server thread (attach-with-server).
    CodexThread,
    /// Claude stream-json adoption/resume lane.
    ClaudeStream,
    /// Mux-hosted interactive pane (the default PTY destination; hosted by
    /// the mux, born through the coordinator).
    MuxPane,
    /// A client-side one-shot (bg / headless); no durable row from the daemon.
    ClientOneShot,
    /// The harness/substrate combination is genuinely unsupported; the reason
    /// names what the caller can do instead.
    Refused(String),
}

/// Resolve (harness, substrate) through the capability contract. An
/// unreadable contract refuses rather than routing blind.
pub fn resolve_backend(harness: &str, substrate: &str) -> ResolvedBackend {
    let contract = match HarnessContract::packaged() {
        Ok(c) => c,
        Err(error) => {
            return ResolvedBackend::Refused(format!(
                "spawn refused: capability table unreadable: {error}"
            ))
        }
    };
    match substrate {
        "thread" => match contract.thread_lane(harness) {
            Ok("attach") => match contract.attach_needs_server(harness) {
                Ok(true) => ResolvedBackend::CodexThread,
                Ok(false) => ResolvedBackend::Refused(format!(
                    "thread spawn refused: harness {harness} hosts its own detached thread \
                     client, so the daemon has no thread to hold for it"
                )),
                Err(e) => ResolvedBackend::Refused(format!(
                    "thread spawn refused: attach_needs_server unreadable for {harness}: {e}"
                )),
            },
            Ok(_) => ResolvedBackend::Refused(format!(
                "thread spawn refused: harness {harness} names no attach-with-server thread \
                 destination"
            )),
            Err(e) => ResolvedBackend::Refused(format!(
                "thread spawn refused: capability table has no thread lane for {harness}: {e}"
            )),
        },
        "pane" => ResolvedBackend::MuxPane,
        "bg" | "headless" => ResolvedBackend::ClientOneShot,
        other => ResolvedBackend::Refused(format!("unknown substrate '{other}'")),
    }
}

//! Session-lifetime guards for the mux server: what must hold while the
//! session lives, and what the server must clean up when it retires -
//! whether by SIGTERM, idle exit, or drift retirement (x-6648). Moved out of
//! `server.rs` (over budget, shrink-only) with no behavior change.

use std::path::PathBuf;

/// Unlink the socket AND both its sidecars (`.ver`, `.pid`) on
/// every exit path out of `run` (a SIGKILL leaves them behind by design; the
/// stale-socket path in `bind_or_probe` covers that, and a lingering `.ver`
/// is inert - `ls` only reads it for a LIVE server, and a dead one probes
/// `Stale`).
pub(crate) struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = crate::proto::remove_session_files(&self.0);
        crate::proto::remove_startup_guard(&self.0);
    }
}

/// RAII count of in-flight connections, read by the FNO_E2E idle reaper. A
/// control one-shot (`pane run`, kill-server, a probe) is NOT an attached
/// client, so without this the reaper can fire mid-verb on a young server
/// whose test grace is shorter than a loaded machine's verb latency, killing
/// the server out from under a live peer.
pub(crate) struct ConnAlive(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ConnAlive {
    fn new(count: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::Release);
        ConnAlive(count.clone())
    }
}

impl Drop for ConnAlive {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

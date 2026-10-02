//! The attach handshake: everything the client reads before the first Layout.

use std::time::Duration;

use super::{LayoutView, View};
use crate::proto::{read_msg, ProtoError, ServerMsg};

/// The typed shape of one failure: the server accepted the attach and then
/// closed the connection before the first Layout. `attach_once_more` keys
/// its single retry on this prefix, and a double failure exits with it.
pub(crate) const PREAMBLE_CLOSED: &str = "server closed before the first layout";

/// How long the attach waits in silence before it tells the user the server
/// is busy. It never gives up: a human attach always gets in.
pub(crate) const ATTACH_BUSY_NOTICE: Duration = Duration::from_secs(10);

/// Await one handshake reply. The first time a read outlasts `notice_after`,
/// `on_busy` runs and the SAME read continues (`read_msg` is not
/// cancellation-safe, so it is never dropped and restarted).
pub(crate) async fn await_attach_reply<F: std::future::Future>(
    read: F,
    notice_after: Duration,
    busy_said: &mut bool,
    on_busy: impl FnOnce(),
) -> F::Output {
    tokio::pin!(read);
    if !*busy_said {
        if let Ok(out) = tokio::time::timeout(notice_after, &mut read).await {
            return out;
        }
        *busy_said = true;
        on_busy();
    }
    read.await
}

/// The first Layout (or refusal) decides everything, BEFORE the terminal
/// is taken over, so a refusal prints as a plain one-liner (AC1-ERR,
/// version skew). ModeSync may precede it on the reliable channel - stash
/// and apply once the TUI owns the terminal. A slow server is still the
/// user's server: past ATTACH_BUSY_NOTICE the client says so once and
/// keeps waiting while the socket stays open. Ctrl-C still works here,
/// since the terminal is not raw yet. Returns the stashed ModeSync bytes.
pub(super) async fn read_preamble<R: tokio::io::AsyncRead + Unpin>(
    sock_r: &mut R,
    view: &mut View,
    log_hint: &str,
) -> Result<Vec<u8>, String> {
    let mut busy_said = false;
    let mut stashed_modesync: Vec<u8> = Vec::new();
    loop {
        let msg = await_attach_reply(
            read_msg::<_, ServerMsg>(&mut *sock_r),
            ATTACH_BUSY_NOTICE,
            &mut busy_said,
            || eprintln!("fno: server is busy, still waiting (Ctrl-C to stop; {log_hint})"),
        )
        .await;
        match msg {
            Ok(ServerMsg::Layout {
                squads,
                active_squad,
                panes,
                focus,
                area,
                agents,
                focus_node,
                backlog,
                ..
            }) => {
                view.set_layout(LayoutView {
                    squads,
                    active_squad,
                    panes,
                    focus,
                    area,
                    agents,
                    focus_node,
                });
                // The launcher's node picker composes over the feed even
                // though the sidebar no longer renders it.
                view.backlog = backlog;
                return Ok(stashed_modesync);
            }
            Ok(ServerMsg::ModeSync { bytes }) => stashed_modesync.extend_from_slice(&bytes),
            Ok(ServerMsg::Bye { reason }) => return Err(reason),
            Ok(ServerMsg::Frame { pane_id, frame }) => {
                // Tolerated out-of-order preamble: keep it; the Layout names
                // its rect a message later. The wire trust boundary holds
                // even here: a geometry-inconsistent frame is refused loudly
                // (like a malformed message), never skipped or drawn.
                if !frame.geometry_ok() {
                    return Err(format!(
                        "malformed frame from server: {}x{} but {} cells",
                        frame.rows,
                        frame.cols,
                        frame.cells.len()
                    ));
                }
                view.frames.insert(pane_id, frame);
            }
            // Info answers a pre-Attach Query; the v4 control-verb replies
            // answer one-shot `fno mux pane` connections. Neither belongs on
            // an attached connection - ignore rather than desync.
            Ok(
                ServerMsg::Notice { .. }
                | ServerMsg::Info { .. }
                | ServerMsg::PaneList { .. }
                | ServerMsg::PaneText { .. }
                | ServerMsg::PaneSpawned { .. }
                | ServerMsg::Ok
                | ServerMsg::WaitDone { .. }
                | ServerMsg::Err { .. }
                // Copy and OpenLink answer a mouse-release, and SearchResult
                // answers a search - all can only follow attach: stray in the
                // preamble, ignore rather than desync. LinkHover answers a
                // hover probe (same class).
                | ServerMsg::Copy { .. }
                | ServerMsg::OpenLink { .. }
                | ServerMsg::SearchResult { .. }
                | ServerMsg::LinkHover { .. }
                // PeekBody answers a post-attach PeekAgent: impossible
                // in the preamble, ignore rather than desync.
                | ServerMsg::PeekBody { .. }
                // (v41) Script-layout control-verb replies: only ever sent on a
                // one-shot control connection, never to an attached client.
                | ServerMsg::TabList { .. }
                | ServerMsg::LayoutTree { .. }
                | ServerMsg::PaneLocation { .. }
                | ServerMsg::TabSpawned { .. }
                | ServerMsg::PaneFocused { .. }
                | ServerMsg::LayoutApplied { .. }
                | ServerMsg::LayoutGrafted { .. }
                | ServerMsg::TabLocation { .. }
                | ServerMsg::TabClosed { .. }
                // (v60/v71/v75/v78) one-shot control-verb replies: never
                // attached-client traffic.
                | ServerMsg::WorkspaceRestored { .. } | ServerMsg::SquadReloaded { .. }
                | ServerMsg::SessionRetired { .. } | ServerMsg::AgentRowsReceipt { .. }
                | ServerMsg::ServerStats { .. },
            ) => {}
            // A launch update cannot precede attach; ignore a misaddressed
            // one rather than failing the handshake.
            Ok(ServerMsg::AgentLaunch(_)) => {}
            Err(ProtoError::Closed) => {
                return Err(format!("{PREAMBLE_CLOSED}; {log_hint}"))
            }
            Err(e) => return Err(format!("attach failed: {e}; {log_hint}")),
        }
    }
}

/// Attach once, and when the server accepted and then closed before the
/// first Layout, once more against a fresh server. The retry line carries
/// the server's own last shutting-down cause from its log, so the user
/// reads why the first attach bounced without opening the log. A second
/// failure exits with the typed message; no third try.
pub(super) async fn attach_once_more(path: &std::path::Path) -> Result<i32, String> {
    let mut closed_once = false;
    loop {
        let stream = super::connect_or_spawn(path, true)?;
        match super::attach_and_run(stream, path).await {
            Ok(code) => return Ok(code),
            Err(e) if !closed_once && e.starts_with(PREAMBLE_CLOSED) => {
                closed_once = true;
                let cause = last_shutdown_line(&super::log_path(path))
                    .unwrap_or_else(|| "no cause in the server's log".to_string());
                eprintln!("fno: the server closed before attach; retrying once ({cause})");
            }
            Err(e) => return Err(e),
        }
    }
}

/// The last `fno mux: shutting down (...)` line in the server's log, read
/// from the log tail so a long-lived log costs a bounded read.
fn last_shutdown_line(log: &std::path::Path) -> Option<String> {
    let data = std::fs::read(log).ok()?;
    let tail = &data[data.len().saturating_sub(65_536)..];
    std::str::from_utf8(tail)
        .ok()?
        .lines()
        .rev()
        .find(|l| l.contains("fno mux: shutting down"))
        .map(str::to_string)
}

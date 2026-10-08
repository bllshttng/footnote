use super::{Core, ServerMsg};

impl Core {
    /// The resolved link under viewport cell `(row, col)` of `pane`, or
    /// `None`. The pane's LIVE cwd (kernel cwd while the child pid resolves,
    /// else its spawn cwd) is what bare relative candidates resolve against,
    /// so click and hover read one pane entry and derive the cwd once.
    pub(super) fn pane_link_span(
        &self,
        pane: u64,
        row: u16,
        col: u16,
    ) -> Option<crate::vt::LinkSpan> {
        let e = self.panes.get(&pane)?;
        let cwd = crate::pane_cwd::live_or_spawn(e.pty.child_pid(), &e.cwd);
        e.vt.link_span(row, col, &cwd)
    }

    /// Send a validated link only to the client that clicked it.
    pub(super) fn send_open_link(&mut self, client_id: u64, url: String) {
        if !(crate::link::is_openable(&url)
            || crate::link::is_sender_uri(&url)
            || crate::link::is_message_uri(&url)
            || crate::link::is_file_uri(&url))
        {
            return;
        }
        let Some(c) = self.clients.iter().find(|c| c.id == client_id) else {
            return;
        };
        if c.reliable_tx.try_send(ServerMsg::OpenLink { url }).is_err() {
            eprintln!(
                "fno mux: client {client_id} reliable channel wedged on OpenLink; dropping it"
            );
            self.clients.retain(|c| c.id != client_id);
            self.push_layout(true);
        }
    }
}

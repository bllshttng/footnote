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
            || crate::link::is_handle_uri(&url)
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

    /// The claim half of an fno-token click on a mouse-owning pane. A LEFT
    /// press whose cell carries an fno token URI (`@handle`, a bare or
    /// headered `fmail-` id) claims the pair instead of forwarding; the
    /// matching release consumes the claim and answers OpenLink, and a
    /// release on any other cell is swallowed with it (the app saw no press,
    /// so it gets no torn tail). Every other event forwards (return `false`)
    /// and drops a pending claim, so a drag or a second press can never
    /// leave a half-owned pair behind. A release for which no claim is held
    /// forwards untouched, so an app-owned click pair stays byte-identical.
    pub(super) fn claim_fno_token_click(
        &mut self,
        client_id: u64,
        pane: u64,
        event: &crate::proto::MouseEvent,
    ) -> bool {
        let token_at = |core: &Self, row: u16, col: u16| {
            core.pane_link_span(pane, row, col)
                .map(|span| span.uri)
                .filter(crate::link::is_fno_uri)
        };
        match event.kind {
            crate::proto::MouseKind::Press(crate::proto::MouseButton::Left) => {
                let claimed = token_at(self, event.row, event.col).is_some();
                self.fno_token_claim = claimed.then(|| (pane, event.row, event.col, client_id));
                claimed
            }
            crate::proto::MouseKind::Release(crate::proto::MouseButton::Left) => {
                match self.fno_token_claim.take() {
                    Some((p, row, col, c)) if p == pane && c == client_id => {
                        // The pair is ours wherever it ended: the app saw no
                        // press, so it gets no torn tail. Same cell is the
                        // click that opens; any other cell was a tiny drag.
                        if (row, col) == (event.row, event.col) {
                            if let Some(url) = token_at(self, event.row, event.col) {
                                self.send_open_link(client_id, url);
                            }
                        }
                        true
                    }
                    other => {
                        self.fno_token_claim = other;
                        false
                    }
                }
            }
            _ => {
                self.fno_token_claim = None;
                false
            }
        }
    }
}

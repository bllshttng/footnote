use super::*;

pub(crate) struct PaneInputRequest {
    pub id: u64,
    pub request_id: u64,
    pub pane: u64,
    pub expected_identity: String,
    pub bytes: Vec<u8>,
    pub agents: Result<Vec<RegistryAgent>, &'static str>,
}

impl Core {
    pub(super) fn handle_input(&mut self, id: u64, bytes: Vec<u8>) {
        let focus = self
            .client_view(id)
            .and_then(|view| self.viewed_tab(view))
            .map(|tab| tab.focus);
        if let Some(focus) = focus {
            if let Some(&holder) = self.claims.get(&focus) {
                if pid_alive(holder) {
                    self.notice(id, "busy: relay");
                    return;
                }
                self.claims.remove(&focus);
            }
            let scrolled = self.panes.get_mut(&focus).is_some_and(|entry| {
                if entry.vt.display_offset() == 0 {
                    false
                } else {
                    entry.vt.scroll_to_bottom();
                    true
                }
            });
            if scrolled {
                self.broadcast_pane(focus);
            }
            if let Some(entry) = self.panes.get(&focus) {
                if let Err(crate::pty::PtyError::Write(error)) = entry.pty.write_input(&bytes) {
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        self.notice(id, "pane not accepting input; keys dropped");
                    }
                }
            }
            self.input_tail(focus, &bytes);
        }
    }

    pub(super) fn handle_pane_input(&mut self, request: PaneInputRequest) {
        let result = self.pane_input(
            request.id,
            request.pane,
            &request.expected_identity,
            &request.bytes,
            request.agents,
        );
        let failed = self
            .clients
            .iter()
            .find(|client| client.id == request.id)
            .is_some_and(|client| {
                let failed = client
                    .reliable_tx
                    .try_send(ServerMsg::PaneInputResult(
                        crate::proto::pane_input::PaneInputResult {
                            request_id: request.request_id,
                            pane_id: request.pane,
                            result,
                        },
                    ))
                    .is_err();
                if !failed {
                    client.notify.notify_one();
                }
                failed
            });
        if failed {
            eprintln!(
                "fno mux: client {} reliable channel wedged on PaneInputResult; dropping it",
                request.id
            );
            self.clients.retain(|client| client.id != request.id);
            self.push_layout(true);
        }
    }

    fn pane_input(
        &mut self,
        client_id: u64,
        pane: u64,
        expected_identity: &str,
        bytes: &[u8],
        agents: Result<Vec<RegistryAgent>, &'static str>,
    ) -> Result<(), String> {
        let visible = self
            .client_view(client_id)
            .and_then(|view| self.viewed_tab(view))
            .is_some_and(|tab| crate::tree::leaves(&tab.root).contains(&pane));
        if !visible {
            return Err("pane is no longer in the current view".into());
        }
        if let Some(&holder) = self.claims.get(&pane) {
            if pid_alive(holder) {
                return Err("busy: relay".into());
            }
            self.claims.remove(&pane);
        }
        // This is direct user input, so it bypasses DND like keyboard input;
        // the fresh identity check and relay claim still gate the write.
        match self.pane_send(pane, bytes, false, Some(expected_identity), agents, true) {
            ServerMsg::Ok => {
                let scrolled = self.panes.get_mut(&pane).is_some_and(|entry| {
                    if entry.vt.display_offset() == 0 {
                        false
                    } else {
                        entry.vt.scroll_to_bottom();
                        true
                    }
                });
                if scrolled {
                    self.broadcast_pane(pane);
                }
                self.input_tail(pane, bytes);
                Ok(())
            }
            ServerMsg::Err { msg, .. } => Err(msg),
            _ => Err("pane input failed".into()),
        }
    }
}

use super::{Core, ServerMsg};

impl Core {
    /// Send a validated link only to the client that clicked it.
    pub(super) fn send_open_link(&mut self, client_id: u64, url: String) {
        if !(crate::link::is_openable(&url)
            || crate::link::is_sender_uri(&url)
            || crate::link::is_message_uri(&url))
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

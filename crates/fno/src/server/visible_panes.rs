//! Which panes a person can see right now: `<mux dir>/<session>.visible.json`.
//!
//! A pane host cannot tell a hidden pane from a shown one, since the pty is
//! fed either way. A program in a pane (the buddy mod) reads this file with
//! its own `FNO_SESSION` and `FNO_PANE` to skip work nobody will see.

use super::*;

impl Core {
    /// Write the panes on screen in any driving client, after a layout push.
    /// An observer (the web bridge) subscribes to every pane, so it is not a
    /// viewer. The file is rewritten only when the set changes.
    pub(super) fn publish_visible_panes(&self) {
        let mut panes: Vec<u64> = self
            .clients
            .iter()
            .filter(|c| !c.passive)
            .flat_map(|c| c.visible.iter().copied())
            .collect();
        panes.sort_unstable();
        panes.dedup();
        let Ok(path) = visible_path(&self.session_name) else {
            return;
        };
        let body = serde_json::json!({ "panes": panes }).to_string();
        if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
            return;
        }
        // Best-effort: a reader that finds no file falls back to its own signal.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn visible_path(session: &str) -> Result<PathBuf, String> {
    crate::proto::socket_path(session).map(|sock| crate::proto::visible_sidecar_path(&sock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_file_names_only_panes_a_driving_client_shows() {
        let mut core = super::super::tests::empty_core();
        let client = |id, passive, visible: &[u64]| Client {
            id,
            reliable_tx: tokio::sync::mpsc::channel(1).0,
            dirty: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            synced_modes: Default::default(),
            view: (1, 1),
            visible: visible.iter().copied().collect(),
            dims: (24, 80),
            passive,
            last_press: None,
        };
        core.clients = vec![
            client(1, false, &[7, 3]),
            client(2, false, &[3, 9]),
            client(3, true, &[7, 3, 9, 11]),
        ];
        let _ = crate::proto::ensure_mux_dir();
        core.publish_visible_panes();
        let path = visible_path(&core.session_name).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"panes":[3,7,9]}"#
        );

        // The last driving client leaves: nothing is on screen.
        core.clients.clear();
        core.publish_visible_panes();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"panes":[]}"#);
    }
}

use serde::{Deserialize, Serialize};

/// How an observer reaches the pane. The sideline and the pane border name it
/// so the driver can tell a browser viewer from a terminal one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ObserverVia {
    /// A browser viewer the driver admitted through `serve --share`.
    Share,
    /// A terminal over ssh: `serve --observe`.
    Ssh,
}

impl ObserverVia {
    /// The word the sideline prints beside the observer's name.
    pub fn label(self) -> &'static str {
        match self {
            ObserverVia::Share => "browser",
            ObserverVia::Ssh => "ssh",
        }
    }
}

/// Sent by a passive client right after its `Attach`: who it is and which pane
/// it watches. The tag lives as long as the connection, so a closed socket
/// ends the presence with no heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverHello {
    pub name: String,
    pub pane: u64,
    pub via: ObserverVia,
    /// False for a presence-only ticket (the share opens one per admitted
    /// viewer): the server sends it no pane frames.
    pub frames: bool,
}

/// One watcher on the `Layout`, for the pane border count and the sideline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObserverRow {
    pub pane_id: u64,
    pub name: String,
    pub via: ObserverVia,
}

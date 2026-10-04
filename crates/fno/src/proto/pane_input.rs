use serde::{Deserialize, Serialize};

/// A user input burst addressed to one pane. The server replies after the PTY accepts it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneInputRequest {
    pub request_id: u64,
    pub pane: u64,
    pub expected_identity: String,
    pub bytes: Vec<u8>,
}

/// The addressed pane write result; only success authorizes the client to journal a reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneInputResult {
    pub request_id: u64,
    pub pane_id: u64,
    pub result: Result<(), String>,
}

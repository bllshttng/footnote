//! The persisted slot family (x-a9b4): what one pane of a stored layout
//! binds to. Extracted from `proto` under the file-budget gate; re-exported
//! there, so every `crate::proto::` path keeps working.

use serde::{Deserialize, Serialize};

/// What a layout slot binds to (v44, x-6928). Exactly one slot binds `Anchor`
/// (the calling pane the subtree replaces); `Fno` reuses a live session's pane;
/// `Shell` is an intentional empty pane. Raw commands are out of scope (Locked
/// Decision 9) - launch stays in the agents subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutBinding {
    Anchor,
    Fno(String),
    Shell,
}

/// (x-a9b4) A persisted portal seat: the portal index and the row key it
/// shows. Additive and `#[serde(default)]` on the slot field: a build
/// without the field ignores it and restores the seat as the plain shell it
/// already is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortalSlot {
    pub index: u8,
    pub row: String,
}

/// A named slot + its binding (v44, x-6928). A `Vec`, not a map: TOML cannot
/// distinguish two empty-table bindings (`Anchor` vs `Shell`) under one key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayoutSlot {
    pub name: String,
    pub binding: LayoutBinding,
    /// (v68, x-5baf) pane's cwd at capture; `None` pre-v68.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// (x-a9b4) When this slot is a portal seat: the portal index and the row
    /// key it shows. The binding stays `Shell` because at restore the seat IS
    /// a shell until the first reach fills it; this pair is what the reach
    /// needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal: Option<PortalSlot>,
}

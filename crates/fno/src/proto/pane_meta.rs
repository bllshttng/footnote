//! `PaneMeta`, one pane inside a [`TabMeta`]: the navigator's goto target
//! plus the display facts the pane frame's edges read (v91). Out of
//! `proto.rs` under the file-budget ratchet; `proto.rs` re-exports the type.

use serde::{Deserialize, Serialize};

/// A pane's frame-edge facts beyond its label. `Option` + `serde(default)`
/// keeps a pre-v91 reader wire-tolerant: absent decodes to `None`, and a row
/// with all three `None` serializes to the v86 bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneMeta {
    pub id: u64,
    pub label: String,
    /// The registry row's node id (`PaneEntry.node`), bottom edge left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The git branch at the pane's cwd (`branch_by_cwd`), bottom edge left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The server-formatted context reading (`49%`, or `98k` when the harness
    /// states no window), bottom edge right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC6-ERR: a pre-v91 row decodes with the new fields None.
    #[test]
    fn a_v86_row_decodes_with_the_new_fields_none() {
        let v: PaneMeta = serde_json::from_str(r#"{"id":7,"label":"shell"}"#).unwrap();
        assert_eq!(v.id, 7);
        assert_eq!(v.label, "shell");
        assert_eq!((v.node, v.branch, v.ctx), (None, None, None));
    }

    // AC6-ERR, other half: all-None serializes to the v86 bytes.
    #[test]
    fn all_none_fields_serialize_to_the_v86_bytes() {
        let v = PaneMeta {
            id: 7,
            label: "shell".into(),
            node: None,
            branch: None,
            ctx: None,
        };
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"id":7,"label":"shell"}"#
        );
    }
}

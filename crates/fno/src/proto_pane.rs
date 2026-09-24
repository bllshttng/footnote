//! One pane's listing row : what `pane ls` and the client's
//! pane list carry. Extracted from `proto` under the file-budget gate;
//! re-exported there, so every `crate::proto::` path keeps working.

use serde::{Deserialize, Serialize};

/// One pane's metadata in a [`crate::proto::ServerMsg::PaneList`]. `cwd` is
/// the squad's canonical root; `child_pid` is `None` only if the OS never
/// reported one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    pub pane_id: u64,
    pub squad_id: u64,
    /// The live workspace name, when this pane belongs to a named workspace.
    /// Additive so workspace maintenance can apply `--include-named` to tabs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squad_name: Option<String>,
    pub tab_id: u64,
    pub cwd: String,
    pub child_pid: Option<u32>,
    pub title: Option<String>,
    /// Positive shell-integrated evidence that this pane is pristine and idle.
    /// Missing/false is never treated as empty by a cleanup caller. A portal
    /// seat never reads pristine, whatever its key resolves to: the seat is
    /// load-bearing, so a cleanup caller must not close it.
    #[serde(default)]
    pub pristine_idle_shell: bool,
    /// (v65) The pane ran something and sits at a prompt NOW: shell
    /// integration measured, no command running, a completed block. Narrower
    /// than `!pristine_idle_shell` (which also covers running and unmeasured
    /// panes): a cleanup caller may close on this, never the bare negation.
    /// `#[serde(default)]` keeps a pre-v65 reader wire-tolerant.
    #[serde(default)]
    pub shell_idle: bool,
    /// (v51) The pane's tab name and 1-based ordinal, so the human
    /// listing prints `tab=<name-or-·N> tab_id=<id>`. `None` mid-teardown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_ordinal: Option<usize>,
    /// (v41, layout-api) The `fno_id` of the session hosting this pane, filled
    /// server-side from the registry join the mux already caches. `None` for a
    /// pane with no registry row (an ad-hoc shell). Powers `pane ls --fno-id`
    /// and the reverse direction of `where` (Locked Decision 6).
    #[serde(default)]
    pub fno_id: Option<String>,
    /// (v71) Hosts a stored member judged Dead; the default prune closes its
    /// tab. `#[serde(default)]`: a v68 payload reads false.
    #[serde(default)]
    pub orphaned_worker: bool,
    /// When the release tier fired, the evidence the release rode:
    /// `reaped <harness> <session id> at <ts>: <basis>`. Additive like
    /// `orphaned_worker`; absent on every other pane and every other tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The joined row's classified lineage: the CURRENT harness
    /// session the row answers as, the succession chain it retired, and the
    /// fork edge of a parallel branch. `fno_id` stays the stable thread join;
    /// these print BESIDE it so a retired id never reads as current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predecessor_session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_session_id: Option<String>,
    /// (v51) The pane's spawn-captured `FNO_AGENT_SELF` identity.
    #[serde(default)]
    pub name: Option<String>,
    /// (v88) The portal index whose seat this pane is, under the same
    /// one-row rule the sideline marker wears: a held seat (its row is gone)
    /// or an ambiguous key carries none. Absent on every other pane, so the
    /// JSON shape of `pane ls --json` for plain panes is byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal: Option<u8>,
}

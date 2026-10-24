//! What wire generation the attached server speaks, and the one command
//! gate that reads it: a directional split must never reach a server that
//! cannot parse it.

/// True when the attached server parses `Command::SplitDir` (v96). A server
/// that never announced (pre-v97 layout) counts as unable: announcing began
/// one generation after the command, and the unannounced builds include the
/// one that ends the client's session on the unknown variant.
pub(super) fn server_has_splitdir(server_proto: Option<u32>) -> bool {
    server_proto.is_some_and(|proto| proto >= 96)
}

/// True when the attached server parses `Command::PinThread` and
/// `Command::AddWorkspaceFolder` (v114). Same reading of "never announced" as
/// the SplitDir gate: unable.
pub(super) fn server_has_workspace_pins(server_proto: Option<u32>) -> bool {
    server_proto.is_some_and(|proto| proto >= 114)
}

/// True when the attached server parses `ClientMsg::PaneScroll` /
/// `PaneScrollEdge` (v118, keyboard scroll mode). Same reading of "never
/// announced" as the SplitDir gate: unable.
pub(super) fn server_has_pane_scroll(server_proto: Option<u32>) -> bool {
    server_proto.is_some_and(|proto| proto >= 118)
}

/// The refusal line a move or an Add folder shows against an older server.
pub(super) fn workspace_pins_skew_notice() -> String {
    "update the mux server to move a session".into()
}

/// The one refusal line both SplitDir entry points (the `%` family and the
/// tab menu) show against a server that cannot parse the command.
pub(super) fn split_skew_notice() -> String {
    "this mux server is older than directional splits; restart the mux server to use them".into()
}

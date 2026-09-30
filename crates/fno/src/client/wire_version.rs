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

/// The one refusal line both SplitDir entry points (the `%` family and the
/// tab menu) show against a server that cannot parse the command.
pub(super) fn split_skew_notice() -> String {
    "this mux server is older than directional splits; restart the mux server to use them".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_has_splitdir_edges() {
        // Only an announced v96+ server admits the command; an
        // unannounced (pre-v97) server counts as unable, since the
        // announcement began one generation after the command.
        assert!(!server_has_splitdir(None));
        assert!(!server_has_splitdir(Some(95)));
        assert!(server_has_splitdir(Some(96)));
        assert!(server_has_splitdir(Some(97)));
    }
}

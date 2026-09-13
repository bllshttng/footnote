//! (x-91ba) The pane-send audit row: the record `fno mux pane send` writes so
//! "who told this worker to do that" is one grep, not a transcript sweep.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::mux_cli::{
    EXIT_CONTROL_UNANSWERED, EXIT_ERROR, EXIT_OK, EXIT_SUBMIT_UNCONFIRMED, EXIT_TARGET_DND,
    EXIT_TARGET_IDENTITY_MISMATCH,
};
use crate::proto::{read_msg_sync, write_msg_sync, ClientMsg, ServerMsg};

/// Serializes tests that point FNO_AGENTS_HOME at a scratch dir; cargo
/// runs this binary's tests on parallel threads against one process env.
/// [`FNO_BIN_GUARD`] is the same deal for tests that repoint FNO_BIN.
#[cfg(test)]
pub(crate) static FNO_AGENTS_HOME_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) static FNO_BIN_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One audit row per prompt write. The mail lane's rows already live in
/// `agent_raw_inject`; the floor reuses that type so a reader greps one kind.
/// The provenance (`source`) is DECLARED by the caller via `--source`, never
/// sniffed from the bytes: `--raw` carries both an already-wrapped mail body
/// and an operator's verbatim keystrokes, which no inspection can tell apart.
pub(crate) struct PaneSendAudit {
    pane: u64,
    expected_identity: Option<String>,
    digest: String,
    payload_bytes: usize,
    preview: String,
    submit: bool,
    provenance: Option<String>,
}

impl PaneSendAudit {
    /// Digest and preview the EXACT bytes about to be typed at the pane
    /// (post-wrap, post-cap), so the row identifies what actually landed.
    pub(crate) fn new(
        pane: u64,
        expected_identity: Option<&str>,
        bytes: &[u8],
        submit: bool,
        provenance: Option<&str>,
    ) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        PaneSendAudit {
            pane,
            expected_identity: expected_identity.map(str::to_string),
            digest: format!("{:x}", hasher.finalize()),
            payload_bytes: bytes.len(),
            preview: String::from_utf8_lossy(bytes).chars().take(512).collect(),
            submit,
            provenance: provenance
                .map(str::to_string)
                .filter(|p| !p.trim().is_empty()),
        }
    }

    pub(crate) fn emit(self, session: &str, exit_code: i32) {
        self.emit_at(&pane_send_audit_events_path(), session, exit_code);
    }

    /// Best-effort, like every other events write: a failed or refused row
    /// must not break the send itself. The envelope and field names match the
    /// Rust mail-inject rows (`EventEmitter`'s unified x-2901 shape).
    fn emit_at(self, path: &Path, session: &str, exit_code: i32) {
        let pid = std::process::id();
        // The occupant identity is resolved from the registry by pane address,
        // never from the payload; a pane with no row simply carries no name.
        let registry = pane_send_registry_identity(session, self.pane);
        let (outcome, confirmed) = pane_send_outcome(exit_code, self.submit);
        let verb: Option<String> = self
            .preview
            .strip_prefix('/')
            .and_then(|p| p.split_whitespace().next())
            .map(str::to_string);
        let mut data = serde_json::Map::new();
        data.insert("lane".into(), "pane-send".into());
        data.insert("target_session".into(), session.to_string().into());
        data.insert("target_pane".into(), self.pane.into());
        if let Some(id) = self
            .expected_identity
            .or_else(|| registry.as_ref().and_then(|(_, _, id)| id.clone()))
        {
            data.insert("target_fno_id".into(), id.into());
        }
        if let Some((name, harness, _)) = &registry {
            data.insert("target_name".into(), name.clone().into());
        }
        // The schema requires `harness` on every row; a pane with no registry
        // row is honestly "unknown", never a blank.
        data.insert(
            "harness".into(),
            registry
                .as_ref()
                .and_then(|(_, harness, _)| harness.clone())
                .unwrap_or_else(|| "unknown".into())
                .into(),
        );
        data.insert("payload".into(), self.preview.into());
        data.insert("payload_sha256".into(), self.digest.into());
        data.insert("payload_bytes".into(), self.payload_bytes.into());
        let source = match &self.provenance {
            Some(label) => label.clone(),
            None => format!("unattributed:{pid}"),
        };
        data.insert("source".into(), source.into());
        if let Some(caller) = std::env::var_os("FNO_SESSION")
            .filter(|v| !v.is_empty())
            .and_then(|v| v.into_string().ok())
        {
            data.insert("caller_session".into(), caller.into());
        }
        data.insert("caller_pid".into(), pid.into());
        data.insert("submit".into(), self.submit.into());
        data.insert("confirmed".into(), confirmed.into());
        data.insert("outcome".into(), outcome.into());
        data.insert("exit_code".into(), exit_code.into());
        data.insert("verb".into(), verb.into());
        let event = serde_json::json!({
            "ts": crate::review_invocation::review_invocation_timestamp(),
            "type": "agent_raw_inject",
            "source": "daemon",
            "data": data,
        });
        let _ = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(file, "{event}")
        })();
    }
}

/// A submit key is a control byte, not a dispatch: the CRs, Tabs and ESC
/// sequences the mail lane issues one per submit key ride this same verb but
/// write no row. A prompt write is never made only of these.
pub(crate) fn pane_send_is_control_only(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if bytes.first() == Some(&0x1b) {
        return true; // an escape sequence (an arrow key, a bare Esc)
    }
    bytes.iter().all(|b| b.is_ascii_control())
}

/// The audit outcome vocabulary. Every pane-send exit code is representable,
/// and exit 22 (text landed, no post-submit marker) is NEVER "delivered" or
/// "submitted": that exact distinction is what the row exists to preserve.
fn pane_send_outcome(exit_code: i32, submit: bool) -> (&'static str, bool) {
    if exit_code == EXIT_OK {
        return (if submit { "submitted" } else { "delivered" }, true);
    }
    let outcome = if exit_code == EXIT_SUBMIT_UNCONFIRMED {
        "unconfirmed"
    } else if exit_code == EXIT_TARGET_IDENTITY_MISMATCH {
        "identity-mismatch"
    } else if exit_code == EXIT_TARGET_DND {
        "dnd"
    } else if exit_code == EXIT_CONTROL_UNANSWERED {
        "unanswered"
    } else {
        "refused"
    };
    (outcome, false)
}

/// The pane occupant's registry row (name, harness, durable identity) for
/// `session:pane`, resolved once per row write. Same file the sideline parses;
/// unreadable means None, never a failed send.
fn pane_send_registry_identity(
    session: &str,
    pane: u64,
) -> Option<(String, Option<String>, Option<String>)> {
    let raw = std::fs::read_to_string(crate::agents_view::registry_path()).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::agents_view::derive_rows(&raw, now)?
        .into_iter()
        .find_map(|row| {
            let hit = row.mux.as_ref().map(|(s, p)| (s.as_str(), *p)) == Some((session, pane));
            hit.then(|| {
                (
                    row.name.clone(),
                    row.harness.clone(),
                    row.effective_identity().map(str::to_string),
                )
            })
        })
}

/// The agents events journal the mail lane and daemon already write
/// (`~/.fno/agents/events.jsonl`, `FNO_AGENTS_HOME` redirects tests), mirrored
/// from fno-agents' `AgentPaths` - the crates share no types, the FILE is the
/// contract.
fn pane_send_audit_events_path() -> PathBuf {
    if let Some(home) = std::env::var_os("FNO_AGENTS_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(&home).join("events.jsonl");
    }
    let base = match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(".fno").join("agents"),
        _ => PathBuf::from(".fno").join("agents"),
    };
    base.join("events.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux_cli::{dispatch, parse_pane_args, PaneCmd, SendSource};
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn control_test_sock(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fno-control-{}-{name}.sock", std::process::id()))
    }

    fn read_audit_rows(path: &std::path::Path, needle: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(needle))
            .map(|line| serde_json::from_str(line).expect("audit row is one JSON object"))
            .collect()
    }

    #[test]
    fn pane_send_parse_source_flag_declares_provenance() {
        assert_eq!(
            parse_pane_args(&os(&["send", "2", "--text", "hi", "--source", "mail"]))
                .unwrap()
                .cmd,
            PaneCmd::Send {
                pane: 2,
                source: SendSource::Text("hi".into()),
                guarded: false,
                submit: false,
                raw: false,
                expected_identity: None,
                style_exception: None,
                provenance: Some("mail".into()),
            }
        );
        // A valueless flag and a non-send verb are usage errors, mirroring
        // --style-exception: a silently ignored flag would read as "the row
        // carried the declared source".
        assert!(parse_pane_args(&os(&["send", "2", "--text", "hi", "--source"])).is_err());
        assert!(parse_pane_args(&os(&["read", "2", "--source", "mail"])).is_err());
    }

    #[test]
    fn pane_send_audit_outcome_vocabulary_never_claims_delivery_for_exit_22() {
        assert_eq!(pane_send_outcome(EXIT_OK, true), ("submitted", true));
        assert_eq!(pane_send_outcome(EXIT_OK, false), ("delivered", true));
        // Exit 22 is the distinction the row exists to preserve: text landed,
        // no post-submit marker. It never reads as delivered or submitted.
        let (outcome, confirmed) = pane_send_outcome(EXIT_SUBMIT_UNCONFIRMED, true);
        assert_eq!(outcome, "unconfirmed");
        assert!(!confirmed);
        assert_ne!(outcome, "delivered");
        assert_ne!(outcome, "submitted");
        assert_eq!(
            pane_send_outcome(EXIT_TARGET_IDENTITY_MISMATCH, true).0,
            "identity-mismatch"
        );
        assert_eq!(pane_send_outcome(EXIT_TARGET_DND, true).0, "dnd");
        assert_eq!(pane_send_outcome(EXIT_ERROR, true).0, "refused");
    }

    #[test]
    fn pane_send_audit_source_is_declared_not_sniffed() {
        // A payload that LOOKS like a wrapped mail body still records the
        // caller-declared source: sniffing would make an operator who pastes
        // that string read as the mail lane (AC3-HP).
        let dir = std::env::temp_dir().join(format!("fno-audit-no-sniff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let body = "<fno_mail>\nfrom: someone\n\nhi\n</fno_mail>";
        PaneSendAudit::new(3, None, body.as_bytes(), false, None).emit_at(&path, "t", EXIT_OK);
        let rows = read_audit_rows(&path, "pane-send");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["type"], "agent_raw_inject");
        let data = &rows[0]["data"];
        assert_eq!(data["lane"], "pane-send");
        assert!(
            data["source"]
                .as_str()
                .unwrap()
                .starts_with("unattributed:"),
            "an undeclared source reads unattributed:<pid>, got {:?}",
            data["source"]
        );
        assert_eq!(data["payload"], body);
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        assert_eq!(data["payload_sha256"], format!("{:x}", hasher.finalize()));
        assert_eq!(data["payload_bytes"], body.len());
        assert_eq!(data["confirmed"], true);
        assert_eq!(data["outcome"], "delivered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pane_send_audit_skips_submit_key_control_bytes() {
        // (AC5) A carriage return is a control byte, not a dispatch: the mail
        // lane's per-key CR/Tab/arrow sends ride this verb and must not each
        // produce a row. The delivered leg proves the send itself still ran.
        let agents_guard = FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-audit-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        std::env::set_var("FNO_AGENTS_HOME", &dir);

        let sock = control_test_sock("audit-control");
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let connections = std::sync::Arc::new(AtomicU32::new(0));
        let connections_srv = connections.clone();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            connections_srv.fetch_add(1, AtomicOrdering::SeqCst);
            let _msg: ClientMsg = read_msg_sync(&mut s).unwrap();
            write_msg_sync(&mut s, &ServerMsg::Ok).unwrap();
        });

        let code = dispatch(
            "t",
            &sock,
            false,
            PaneCmd::Send {
                pane: 7,
                source: SendSource::Text("\r".into()),
                guarded: false,
                submit: false,
                raw: true,
                expected_identity: None,
                style_exception: None,
                provenance: Some("mail:msg-abc123".into()),
            },
        );
        server.join().unwrap();
        assert_eq!(code, EXIT_OK, "the CR send itself still delivers");
        assert_eq!(
            connections.load(AtomicOrdering::SeqCst),
            1,
            "the CR send reached the socket"
        );
        // ESC + '[' + 'D' (a left-arrow) is control-only by the ESC-prefix
        // rule, though '[' and 'D' are printable bytes.
        assert!(pane_send_is_control_only(b"\x1b[D"));
        assert!(pane_send_is_control_only(b"\t"));
        assert!(!pane_send_is_control_only(b"1"));
        assert!(!pane_send_is_control_only(b""));

        let rows = read_audit_rows(&events, "pane-send");
        assert!(
            !rows
                .iter()
                .any(|row| row["data"]["payload"] == "\r"
                    || row["data"]["source"] == "mail:msg-abc123"),
            "a submit-key send writes no dispatch row"
        );

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(agents_guard);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pane_send_audit_writes_one_row_per_dispatch_on_land_and_refuse() {
        let agents_guard = FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let fno_bin_guard = FNO_BIN_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-audit-dispatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        std::env::set_var("FNO_AGENTS_HOME", &dir);

        // The delivered leg: one fake server, one raw send, one row.
        let sock = control_test_sock("audit-rows");
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let connections = std::sync::Arc::new(AtomicU32::new(0));
        let connections_srv = connections.clone();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            connections_srv.fetch_add(1, AtomicOrdering::SeqCst);
            let _msg: ClientMsg = read_msg_sync(&mut s).unwrap();
            write_msg_sync(&mut s, &ServerMsg::Ok).unwrap();
        });

        let delivered = dispatch(
            "t",
            &sock,
            false,
            PaneCmd::Send {
                pane: 7,
                source: SendSource::Text("audit probe".into()),
                guarded: false,
                submit: false,
                raw: true,
                expected_identity: None,
                style_exception: None,
                provenance: None,
            },
        );
        server.join().unwrap();
        assert_eq!(delivered, EXIT_OK);

        // The refused leg: a renderer that refuses the way the style gate
        // does. It must record the attempt AND reach no socket.
        let script = std::env::temp_dir().join(format!(
            "fno-audit-refused-renderer-{}.sh",
            std::process::id()
        ));
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'pane send refused: rule 1' >&2\nexit 1\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("FNO_BIN", &script);
        let refused = dispatch(
            "t",
            &sock,
            false,
            PaneCmd::Send {
                pane: 7,
                source: SendSource::Text("you should fix this; it breaks.".into()),
                guarded: false,
                submit: false,
                raw: false,
                expected_identity: None,
                style_exception: None,
                provenance: Some("mail".into()),
            },
        );
        std::env::remove_var("FNO_BIN");
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&sock);
        assert_eq!(refused, EXIT_ERROR);
        assert_eq!(
            connections.load(AtomicOrdering::SeqCst),
            1,
            "only the delivered leg may open a control connection"
        );

        // The events file is process-global while this test holds the env:
        // concurrent sends from sibling tests also land here, so each leg is
        // read back by its own payload needle, never by the file's total.
        let delivered_rows = read_audit_rows(&events, "audit probe");
        assert_eq!(
            delivered_rows.len(),
            1,
            "exactly one audit row for the delivered dispatch"
        );
        let delivered_row = &delivered_rows[0];
        assert_eq!(delivered_row["data"]["lane"], "pane-send");
        assert_eq!(delivered_row["data"]["target_pane"], 7);
        assert_eq!(delivered_row["data"]["payload"], "audit probe");
        assert_eq!(delivered_row["data"]["outcome"], "delivered");
        assert_eq!(delivered_row["data"]["confirmed"], true);
        assert!(delivered_row["data"]["source"]
            .as_str()
            .unwrap()
            .starts_with("unattributed:"));
        let refused_rows = read_audit_rows(&events, "you should fix this");
        assert_eq!(
            refused_rows.len(),
            1,
            "exactly one audit row for the refused dispatch"
        );
        let refused_row = &refused_rows[0];
        assert_eq!(refused_row["data"]["outcome"], "refused");
        assert_eq!(refused_row["data"]["source"], "mail");
        assert_eq!(refused_row["data"]["confirmed"], false);
        assert_eq!(refused_row["data"]["exit_code"], EXIT_ERROR);

        std::env::remove_var("FNO_AGENTS_HOME");
        drop(agents_guard);
        drop(fno_bin_guard);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

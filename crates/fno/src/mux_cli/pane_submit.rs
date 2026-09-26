//! Identity-aware pane submission primitives shared by pane send and command.

use super::*;

pub(super) fn positive_post_submit_marker(before_cr: &str, after_cr: &str) -> bool {
    !after_cr.trim().is_empty() && after_cr != before_cr
}

pub(super) fn pane_text(sock: &Path, session: &str, pane: u64) -> Result<String, ControlError> {
    match control_roundtrip(
        sock,
        session,
        ControlVerb::PaneRead {
            pane,
            lines: None,
            block: None,
        },
    )? {
        ServerMsg::PaneText { text, .. } => Ok(text),
        ServerMsg::Err { msg, .. } => Err(ControlError::Fatal(msg)),
        other => Err(ControlError::Fatal(format!(
            "unexpected pane read reply while confirming submit: {other:?}"
        ))),
    }
}

pub(super) fn send_pane_bytes(
    sock: &Path,
    session: &str,
    pane: u64,
    bytes: Vec<u8>,
    guarded: bool,
    expected_identity: Option<&str>,
) -> Result<(), ControlError> {
    match control_roundtrip(
        sock,
        session,
        ControlVerb::PaneSend {
            pane,
            bytes,
            guarded,
            expected_identity: expected_identity.map(str::to_string),
        },
    )? {
        ServerMsg::Ok => Ok(()),
        ServerMsg::Err { code, msg }
            if code == err_code::TARGET_IDENTITY_MISMATCH || code == err_code::TARGET_DND =>
        {
            Err(ControlError::FatalCode { code, msg })
        }
        ServerMsg::Err { msg, .. } => Err(ControlError::Fatal(msg)),
        other => Err(ControlError::Fatal(format!(
            "unexpected pane send reply while submitting: {other:?}"
        ))),
    }
}

pub(super) fn submit_pane(
    sock: &Path,
    session: &str,
    pane: u64,
    bytes: Vec<u8>,
    guarded: bool,
    expected_identity: Option<&str>,
    json: bool,
) -> i32 {
    if let Err(e) = send_pane_bytes(sock, session, pane, bytes, guarded, expected_identity) {
        eprintln!("fno mux pane: {e}");
        return match e {
            ControlError::Unanswered(_) => EXIT_CONTROL_UNANSWERED,
            ControlError::Fatal(_) => EXIT_ERROR,
            ControlError::FatalCode { code, .. } if code == err_code::TARGET_IDENTITY_MISMATCH => {
                EXIT_TARGET_IDENTITY_MISMATCH
            }
            ControlError::FatalCode { code, .. } if code == err_code::TARGET_DND => EXIT_TARGET_DND,
            ControlError::FatalCode { .. } => EXIT_ERROR,
        };
    }
    std::thread::sleep(Duration::from_millis(CR_SETTLE_MS));
    let baseline = pane_text(sock, session, pane).ok();
    if let Err(e) = send_pane_bytes(sock, session, pane, vec![b'\r'], false, expected_identity) {
        eprintln!("fno mux pane: text delivered, submission unconfirmed: {e}");
        if let ControlError::FatalCode { code, .. } = e {
            if code == err_code::TARGET_IDENTITY_MISMATCH {
                return EXIT_TARGET_IDENTITY_MISMATCH;
            }
            if code == err_code::TARGET_DND {
                return EXIT_TARGET_DND;
            }
        }
        return EXIT_SUBMIT_UNCONFIRMED;
    }
    for attempt in 0..SUBMIT_CONFIRM_ATTEMPTS {
        if let (Some(before), Ok(after)) = (baseline.as_deref(), pane_text(sock, session, pane)) {
            if positive_post_submit_marker(before, &after) {
                if !json {
                    println!("submitted");
                }
                return render_reply(ServerMsg::Ok, json, false, None);
            }
        }
        std::thread::sleep(Duration::from_millis(SUBMIT_CONFIRM_INTERVAL_MS));
        if (attempt + 1) % CR_RESUBMIT_EVERY == 0 {
            let _ = send_pane_bytes(sock, session, pane, vec![b'\r'], false, expected_identity);
        }
    }
    eprintln!("fno mux pane: text delivered, submission unconfirmed");
    EXIT_SUBMIT_UNCONFIRMED
}

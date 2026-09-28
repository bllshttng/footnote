//! The peek-overlay mail composer's key adapter (move from
//! `client.rs`, for the shrink-only ratchet). Behavior is preserved
//! byte-for-byte: [`super::fold_search_input`] discipline, Esc drops the
//! buffer, backspace pops, printable ASCII appends, and the mode is re-read
//! per key.

use super::{
    fold_search_input, raw_out, write_msg, ClientMsg, Command, SearchKey, StdinFlow, View,
    MAX_MAIL_TEXT,
};

/// One chunk of keys while the `m` reply input is open. Enter-with-text
/// sends [`Command::MailAgent`] then closes the input, leaving peek open (the
/// notice line is the feedback). The buffer caps at [`MAX_MAIL_TEXT`] chars so
/// the operator sees the same ceiling the server enforces.
pub(super) async fn peek_input_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.peek_input_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.peek_input_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc/Enter mid-chunk closes the input, and
        // the rest of the chunk must be swallowed, never forwarded.
        if view.peek_input.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                // Drop half-typed text; peek stays open underneath (AC parity
                // with rename Esc).
                view.peek_input = None;
                view.peek_input_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    // Empty (or whitespace-only) buffer: BEL, input stays open,
                    // nothing sent (AC3-UI). Otherwise send + close.
                    let send = view
                        .peek_input
                        .as_ref()
                        .filter(|(_, buf)| !buf.trim().is_empty())
                        .map(|(name, buf)| (name.clone(), buf.clone()));
                    match send {
                        None => {
                            let _ = raw_out(b"\x07");
                        }
                        Some((name, text)) => {
                            view.peek_input = None;
                            view.peek_input_esc.clear();
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::MailAgent { name, text }),
                            )
                            .await
                            .map_err(|e| format!("mail send failed: {e}"))?;
                        }
                    }
                    break;
                }
                0x7f | 0x08 => {
                    if let Some((_, buf)) = view.peek_input.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some((_, buf)) = view.peek_input.as_mut() {
                        // Cap to the server's ceiling so the operator sees exactly
                        // what will be accepted (server stays authoritative). Only
                        // printable ASCII is ever pushed, so byte len == char count.
                        if buf.len() < MAX_MAIL_TEXT {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

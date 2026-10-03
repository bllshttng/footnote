//! The peek mail composer's draft, persisted per target row under the mux
//! dir. A portal close or a client death no longer takes the typed text:
//! the next composer open on that row restores it. An Esc or a send is the
//! deliberate end and deletes the file; an overlay open over the composer
//! only drops the memory copy.

use crate::proto::{mux_dir, MAX_MAIL_TEXT};
use std::path::PathBuf;

fn draft_path(target: &str) -> PathBuf {
    // Percent-escape everything outside the safe set, so two distinct
    // target names can never land on one file (a space and an underscore
    // used to collide under a blanket `_` rewrite).
    let mut safe = String::new();
    for b in target.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => safe.push(b as char),
            _ => {
                safe.push('%');
                safe.push_str(&format!("{b:02X}"));
            }
        }
    }
    mux_dir().join("composer-drafts").join(safe)
}

/// The stored draft for `target`, capped to the send ceiling so an old or
/// oversized file can never come back as text the composer would refuse.
pub(crate) fn load(target: &str) -> String {
    std::fs::read_to_string(draft_path(target))
        .unwrap_or_default()
        .chars()
        .take(MAX_MAIL_TEXT)
        .collect()
}

pub(crate) fn save(target: &str, text: &str) {
    if let Some(parent) = draft_path(target).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(draft_path(target), text);
}

pub(crate) fn delete(target: &str) {
    let _ = std::fs::remove_file(draft_path(target));
}

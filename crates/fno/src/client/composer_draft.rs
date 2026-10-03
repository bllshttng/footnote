//! The peek mail composer's draft, persisted per target row under the mux
//! dir. A portal close or a client death no longer takes the typed text:
//! the next composer open on that row restores it. An Esc or a send is the
//! deliberate end and deletes the file; an overlay open over the composer
//! only drops the memory copy.

use crate::proto::{mux_dir, MAX_MAIL_TEXT};
use std::path::PathBuf;

fn draft_path(target: &str) -> PathBuf {
    let safe: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_delete_round_trip_per_target() {
        save("candor", "hello there");
        assert_eq!(load("candor"), "hello there");
        // Another target's draft never bleeds across.
        save("other", "x");
        assert_eq!(load("candor"), "hello there");
        delete("candor");
        assert_eq!(load("candor"), "");
        assert_eq!(load("other"), "x");
        delete("other");
    }

    #[test]
    fn unsafe_target_chars_land_in_one_safe_name() {
        save("a/b c", "kept");
        assert_eq!(load("a/b c"), "kept");
        assert_eq!(load("a_b_c"), "kept", "the sanitized name is the file");
        delete("a/b c");
        assert_eq!(load("a/b c"), "");
    }

    #[test]
    fn load_caps_the_buffer_at_the_send_ceiling() {
        let big: String = "x".repeat(MAX_MAIL_TEXT + 10);
        save("big", &big);
        assert_eq!(load("big").chars().count(), MAX_MAIL_TEXT);
        delete("big");
    }
}

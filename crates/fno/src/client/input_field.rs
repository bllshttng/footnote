//! One text field for every settings sub-page: the typed text, a cursor,
//! a placeholder, and the esc/paste carry. The caller rebuilds the modal after
//! every chunk it feeds, so the painted row always shows the text so far.

use super::agent_launcher::{LKey, LauncherEsc};
use crate::popup::PopupRow;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputField {
    text: String,
    /// A char index into `text`; `text.chars().count()` is past the end.
    cursor: usize,
    placeholder: &'static str,
    max_chars: usize,
    carry: LauncherEsc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldEvent {
    /// Enter, with the trimmed text. The field stays open.
    Submit(String),
    Cancel,
}

impl InputField {
    pub(crate) fn new(placeholder: &'static str, max_chars: usize) -> Self {
        InputField {
            placeholder,
            max_chars,
            ..Default::default()
        }
    }

    /// A field that opens already holding `text` (a refused submit returns
    /// the user to what they typed).
    pub(crate) fn with_text(mut self, text: &str) -> Self {
        self.insert(text);
        self
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Fold one read. An empty read is the quiet-window flush: only it
    /// releases a held lone ESC as Cancel, because the settings path is fed
    /// raw and a trailing ESC on a real read may start a split arrow.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<FieldEvent> {
        let mut events = Vec::new();
        for key in self.carry.fold_carry(bytes, bytes.is_empty()) {
            match key {
                LKey::Char(c) => self.insert(&c.to_string()),
                LKey::Paste(s) => self.insert(&s.replace(['\r', '\n'], "")),
                LKey::Backspace if self.cursor > 0 => {
                    self.cursor -= 1;
                    let at = self.byte_at(self.cursor);
                    self.text.remove(at);
                }
                LKey::KillLeft => {
                    let at = self.byte_at(self.cursor);
                    let start = self.text[..at].rfind('\n').map_or(0, |b| b + 1);
                    if start < at {
                        self.text.replace_range(start..at, "");
                        self.cursor = self.text[..start].chars().count();
                    }
                }
                LKey::Left => self.cursor = self.cursor.saturating_sub(1),
                LKey::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
                LKey::Enter | LKey::CtrlJ => {
                    events.push(FieldEvent::Submit(self.text.trim().to_string()))
                }
                LKey::Esc => events.push(FieldEvent::Cancel),
                _ => {}
            }
        }
        events
    }

    pub(crate) fn row(&self, label: &str) -> PopupRow {
        PopupRow::Input {
            label: label.to_string(),
            text: self.text.clone(),
            cursor: self.cursor,
            placeholder: self.placeholder.to_string(),
        }
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars() {
            if self.text.chars().count() >= self.max_chars {
                break;
            }
            let at = self.byte_at(self.cursor);
            self.text.insert(at, c);
            self.cursor += 1;
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map_or(self.text.len(), |(i, _)| i)
    }
}

/// The `‹ back` row every drilled settings page starts with.
pub(crate) fn back_row() -> PopupRow {
    PopupRow::Entry {
        glyph: "‹".into(),
        label: "back".into(),
        hint: String::new(),
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::{Anchor, Popup};
    use crate::theme::Role;

    #[test]
    fn edits_paste_cursor_and_lone_esc() {
        let mut f = InputField::new("new key name", 64).with_text("zai");
        f.feed(b"\x1b[D\x1b[D"); // two lefts: cursor after "z"
        f.feed(b"x\x1b[200~a\nb\x1b[201~");
        assert_eq!(f.text(), "zxabai");
        assert_eq!(f.cursor, 4, "the cursor sits after the paste");
        assert!(f.feed(b"\x7f").is_empty());
        assert_eq!(f.text(), "zxaai");
        // A read ending in a lone ESC may be a split arrow: held, not Cancel.
        assert!(f.feed(b"\x1b").is_empty());
        assert_eq!(f.feed(b""), vec![FieldEvent::Cancel]);
        assert_eq!(f.feed(b"\r"), vec![FieldEvent::Submit("zxaai".into())]);

        // An empty field paints the cursor cell, then the placeholder dimmed.
        let empty = InputField::new("new key name", 64);
        let r = Popup::new(vec![empty.row("model key")], Anchor::Center)
            .plain_body()
            .render((20, 60));
        let line = r
            .lines
            .iter()
            .find(|l| l.text.contains("model key:"))
            .expect("the field row renders");
        let at = |needle: &str| {
            line.text.chars().count() - line.text[line.text.find(needle).unwrap()..].chars().count()
        };
        let ph = at("new key name");
        assert_eq!(
            line.roles[ph - 1],
            Role::BodyCursor,
            "cursor cell before the placeholder"
        );
        assert_eq!(line.roles[ph], Role::PanelMeta, "placeholder is dimmed");
    }
}

/// Clear an optional single-buffer input on Ctrl+U (Cmd+Backspace): the
/// one-line arm every buffer-style overlay in `client.rs` shares.
pub(crate) fn clear_opt(slot: Option<&mut String>) {
    if let Some(buf) = slot {
        buf.clear();
    }
}

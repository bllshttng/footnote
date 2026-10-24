//! Best-effort masking of secrets and home paths in pane text before it
//! leaves the machine. Over-masking is the safe direction: the generic
//! 40-character run also masks git shas and long base64 blobs.
//!
//! The intel report scrub in fno-agents (`intel_html.rs`) carries the same
//! shapes. fno-agents links this crate only as a dev-dependency until the two
//! crates release together, so it cannot import these yet.

use regex::Regex;
use std::sync::OnceLock;

use crate::proto::Frame;

/// The mask glyph. One per masked character, so a masked line keeps its width.
const MASK: char = '\u{2022}';

/// The secret shapes. Leftmost-first: the named shapes win before the generic
/// 40-character run can touch them. In a `key=value` match, group 2 is the key
/// prefix and group 3 the value. In a `scheme://user:pass@` match, group 4 is
/// the prefix through the colon and group 5 the password.
const SECRET_SHAPES: &str = concat!(
    "(?i)(",
    "-----BEGIN [A-Z ]*PRIVATE KEY-----",
    "|eyJ[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+",
    "|sk-[A-Za-z0-9_-]{20,}",
    "|gh[pousr]_[A-Za-z0-9]{20,}",
    "|github_pat_[A-Za-z0-9_]{20,}",
    "|AKIA[0-9A-Z]{16}",
    "|xox[abprs]-[A-Za-z0-9-]{10,}",
    "|bearer\\s+\\S{16,}",
    "|((?:password|passwd|secret|token|api[_-]?key)\\s*[=:]\\s*)(\\S+)",
    "|([a-z][a-z0-9+.-]*://[^\\s:/@]*:)([^\\s@/]+)@",
    "|[A-Za-z0-9+/_=-]{40,}",
    ")"
);

pub fn secret_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(SECRET_SHAPES).expect("static regex"))
}

pub fn path_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"(?:/Users|/home)/[A-Za-z0-9._-]+").expect("static regex"))
}

/// Byte ranges of `text` to mask: each secret value (the key prefix of a
/// `key=value` match stays) and the user segment of each home path.
fn mask_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for caps in secret_rx().captures_iter(text) {
        let m = caps
            .get(3)
            .or_else(|| caps.get(5))
            .or_else(|| caps.get(0))
            .expect("group 0");
        out.push((m.start(), m.end()));
    }
    for m in path_rx().find_iter(text) {
        let slash = m.as_str()[1..].find('/').map_or(0, |i| i + 2);
        out.push((m.start() + slash, m.end()));
    }
    out
}

/// Mask a block of plain text. Each masked character becomes one mask glyph.
pub fn redact_text(text: &str) -> String {
    let ranges = mask_ranges(text);
    text.char_indices()
        .map(|(b, c)| {
            if c != '\n' && ranges.iter().any(|&(s, e)| b >= s && b < e) {
                MASK
            } else {
                c
            }
        })
        .collect()
}

/// Mask a frame in place, row by row. Only `c` changes; colors and flags stay.
/// A secret wrapped across two rows is not masked: matching runs per row, and
/// joining wrapped rows is the upgrade.
pub fn redact_frame(frame: &mut Frame) {
    if !frame.geometry_ok() || frame.cols == 0 {
        return;
    }
    let cols = frame.cols as usize;
    for row in frame.cells.chunks_mut(cols) {
        let mut text = String::with_capacity(cols);
        let mut starts = Vec::with_capacity(cols);
        for cell in row.iter() {
            starts.push(text.len());
            text.push(cell.c);
        }
        for (s, e) in mask_ranges(&text) {
            for (i, &b) in starts.iter().enumerate() {
                if b >= s && b < e {
                    row[i].c = MASK;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Cell;

    fn frame_of(lines: &[&str], cols: u16) -> Frame {
        let mut cells = Vec::new();
        for line in lines {
            let mut chars: Vec<char> = line.chars().collect();
            chars.resize(cols as usize, ' ');
            cells.extend(chars.into_iter().map(|c| Cell {
                c,
                flags: 3,
                ..Cell::default()
            }));
        }
        Frame {
            rows: lines.len() as u16,
            cols,
            cells,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            scroll_offset: 0,
            history_lines: 0,
        }
    }

    fn row_text(f: &Frame, r: usize) -> String {
        let cols = f.cols as usize;
        f.cells[r * cols..(r + 1) * cols]
            .iter()
            .map(|c| c.c)
            .collect()
    }

    #[test]
    fn frame_and_text_masking_hide_planted_secrets_and_keep_the_rest() {
        let ant = "sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0";
        let ghp = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        let lines = [
            format!("key {ant} ok"),
            format!("tok {ghp}"),
            "cd /Users/alice/repo".to_string(),
            "token=hunter2hunter2 done".to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut f = frame_of(&refs, 80);
        let before = f.clone();
        redact_frame(&mut f);
        assert!(f.geometry_ok());
        let all: String = (0..4)
            .map(|r| row_text(&f, r))
            .collect::<Vec<_>>()
            .join("\n");
        for planted in [ant, ghp, "alice", "hunter2"] {
            assert!(!all.contains(planted), "{planted} leaked: {all}");
        }
        assert!(row_text(&f, 0).starts_with("key "));
        assert!(row_text(&f, 0).trim_end().ends_with(" ok"));
        assert!(row_text(&f, 2).starts_with("cd /Users/\u{2022}"));
        assert!(row_text(&f, 2).trim_end().ends_with("/repo"));
        assert!(row_text(&f, 3).starts_with("token=\u{2022}"));
        // Only glyphs change: unmasked cells are identical, and flags stay.
        for (a, b) in before.cells.iter().zip(&f.cells) {
            assert_eq!(a.flags, b.flags);
            assert!(a == b || b.c == MASK);
        }

        let text = redact_text(&lines.join("\n"));
        for planted in [ant, ghp, "alice", "hunter2"] {
            assert!(!text.contains(planted), "{planted} leaked: {text}");
        }
        assert!(text.contains("cd /Users/\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}/repo"));
        assert!(text.contains("token=\u{2022}"));
    }

    #[test]
    fn url_passwords_are_masked_and_the_rest_of_the_url_stays() {
        for (url, password) in [
            (
                "postgres://demo:hunter2-fake@localhost:5432/demo",
                "hunter2-fake",
            ),
            ("redis://:s3cr3t-fake@cache.internal:6379/0", "s3cr3t-fake"),
            (
                "https://deploy:ghx-fake-pw@git.example.com/org/repo.git",
                "ghx-fake-pw",
            ),
        ] {
            let line = format!("DATABASE_URL={url} ok");
            let got = redact_text(&line);
            assert!(!got.contains(password), "{password} leaked: {got}");
            let (user_part, host_part) = url.split_once(password).unwrap();
            assert!(got.contains(user_part), "scheme and user kept: {got}");
            assert!(
                got.ends_with(&format!("{host_part} ok")),
                "host kept: {got}"
            );
            assert_eq!(got.chars().count(), line.chars().count(), "width kept");
        }
        let plain = "see https://example.com:8080/a@b and http://host:80/x";
        assert_eq!(redact_text(plain), plain, "no userinfo, nothing masked");
    }
}

//! The codex index rollback: if rm's harness cascade dropped the session's
//! `session_index.jsonl` lines and the registry write then fails, put the
//! lines back so the stores agree while the row is still on the board.
//!
//! This is the one arm the deleted Python rm twin had that the Rust cascade
//! lacked. Without it a failed registry write leaves the harness record gone
//! and the row retained - recoverable by a retry (the retry reads
//! already-absent and proceeds), but the stores disagree until that retry.

use crate::state::RegistryEntry;

/// The lines captured before the cascade, ready to restore. Empty for every
/// non-codex row and for codex rows whose index cannot be read.
pub(crate) struct CodexIndexCapture {
    lines: Vec<String>,
}

impl CodexIndexCapture {
    /// Read the index lines whose parsed session id equals this row's, BEFORE
    /// the cascade removes them. Codex rows only.
    pub(crate) fn before_cascade(entry: &RegistryEntry) -> Self {
        if entry.harness_name() != "codex" {
            return Self { lines: Vec::new() };
        }
        let Some(sid) = entry.harness_session_id.as_deref() else {
            return Self { lines: Vec::new() };
        };
        let Some(codex_dir) = crate::client_verbs::codex_home() else {
            return Self { lines: Vec::new() };
        };
        Self {
            lines: read_matching_lines(&codex_dir.join("session_index.jsonl"), sid),
        }
    }

    /// Re-append the captured lines after a failed registry write. Best-effort
    /// and coarse: the append can duplicate an entry another actor re-added in
    /// the window, so a failed restore names itself on stderr and the caller's
    /// refusal carries either way.
    pub(crate) fn restore_on_registry_failure(&self) {
        let Some(codex_dir) = crate::client_verbs::codex_home() else {
            return;
        };
        self.restore_inner(&codex_dir.join("session_index.jsonl"));
    }

    fn restore_inner(&self, index: &std::path::Path) {
        if self.lines.is_empty() {
            return;
        }
        let existing = std::fs::read_to_string(index).unwrap_or_default();
        let missing: Vec<&String> = self
            .lines
            .iter()
            .filter(|line| !existing.contains(line.as_str()))
            .collect();
        if missing.is_empty() {
            return;
        }
        let mut body = String::new();
        for line in missing {
            body.push_str(line);
            body.push('\n');
        }
        if let Err(error) = {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(index)
                .and_then(|mut file| file.write_all(body.as_bytes()))
        } {
            eprintln!(
                "fno-agents: codex index rollback failed for {}: {error}; \
                 the session record may need `fno agents adopt` or a manual re-add",
                index.display()
            );
        }
    }
}

/// The capture half, path-injected for tests. Parse discipline mirrors
/// `cascade_codex_index`: the parsed session id, never substring; lines that
/// fail to parse are never captured.
fn read_matching_lines(index: &std::path::Path, sid: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(index) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| {
            matches!(
                serde_json::from_str::<serde_json::Value>(line),
                Ok(ref value) if value.get("session_id").and_then(|s| s.as_str()) == Some(sid)
            )
        })
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_takes_only_the_rows_lines_and_restore_puts_them_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = tmp.path().join("session_index.jsonl");
        std::fs::write(
            &index,
            concat!(
                "{\"session_id\":\"aaa\",\"path\":\"a.jsonl\"}\n",
                "{\"session_id\":\"bbb\",\"path\":\"b.jsonl\"}\n",
                "not json at all\n",
            ),
        )
        .unwrap();

        let captured = read_matching_lines(&index, "bbb");
        assert_eq!(captured.len(), 1, "only the matching line is captured");
        assert!(captured[0].contains("bbb"));

        // The cascade drops the line; the restore re-appends it verbatim.
        std::fs::write(&index, "{\"session_id\":\"aaa\",\"path\":\"a.jsonl\"}\n").unwrap();
        let restore = CodexIndexCapture { lines: captured };
        restore.restore_inner(&index);
        let text = std::fs::read_to_string(&index).unwrap();
        assert!(text.contains("\"bbb\""), "the line is back: {text}");
        assert!(text.contains("\"aaa\""), "the untouched line stays: {text}");
    }

    #[test]
    fn restore_of_an_empty_capture_is_a_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = tmp.path().join("session_index.jsonl");
        std::fs::write(&index, "{\"session_id\":\"aaa\"}\n").unwrap();
        let before = std::fs::read_to_string(&index).unwrap();
        let restore = CodexIndexCapture { lines: Vec::new() };
        restore.restore_inner(&index);
        assert_eq!(std::fs::read_to_string(&index).unwrap(), before);
    }
}

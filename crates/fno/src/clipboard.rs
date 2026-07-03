//! Client-side clipboard delivery (brief Locked 5, US2). Copy text is extracted
//! server-side and arrives as `ServerMsg::Copy`; this module lands it on the
//! system clipboard via a local exec (`pbcopy`/`wl-copy`/`xclip`/`xsel`, first
//! found), falling back to an OSC 52 escape written to the outer terminal.
//! Failure is reported, never silent (AC2-ERR). The one undetectable case (a
//! terminal that silently drops OSC 52) is what `fno mux doctor` pre-flags
//! (US6, AC6-EDGE); here the OSC 52 write is treated as best-effort success.

use std::io::Write;
use std::process::{Command, Stdio};

/// Where a copy landed, for the client's status feedback.
#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// A local clipboard tool accepted the text (named for the status line).
    Local(&'static str),
    /// No local tool; an OSC 52 escape was emitted to the terminal. `truncated`
    /// = the selection exceeded the conservative payload cap.
    Osc52 { truncated: bool },
    /// Neither path delivered (no tool AND the OSC 52 write itself errored).
    Failed,
}

/// A local clipboard tool and its args, in preference order: macOS `pbcopy`,
/// then Wayland, then the two X11 variants.
const TOOLS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["-i", "-b"]),
];

/// Conservative base64 payload cap for the OSC 52 fallback (~74 KB; larger
/// payloads are dropped by some terminals). A longer selection is head-kept and
/// flagged truncated - never silently dropped (brief Boundaries).
const OSC52_B64_CAP: usize = 74_000;

/// Deliver `text`: try each local tool; on none/all-fail, emit the OSC 52
/// payload via `emit` (the caller writes it to the outer terminal - injected so
/// this stays testable without a TTY).
pub fn deliver<F>(text: &str, emit: F) -> CopyOutcome
where
    F: FnOnce(&[u8]) -> std::io::Result<()>,
{
    deliver_with(copy_via_tool, text, emit)
}

/// The delivery decision with the local-tool attempt injected, so tests can
/// force the no-tool fallback and the both-paths-fail case deterministically
/// (clearing PATH is unreliable: `execvp` falls back to a default system path).
fn deliver_with<L, F>(local: L, text: &str, emit: F) -> CopyOutcome
where
    L: FnOnce(&str) -> Option<&'static str>,
    F: FnOnce(&[u8]) -> std::io::Result<()>,
{
    if let Some(name) = local(text) {
        return CopyOutcome::Local(name);
    }
    let (payload, truncated) = osc52_payload(text);
    match emit(&payload) {
        Ok(()) => CopyOutcome::Osc52 { truncated },
        Err(_) => CopyOutcome::Failed,
    }
}

/// Try each tool in order; return the first whose process took the text
/// (spawned, stdin written, exited 0). A tool absent from PATH or exiting
/// non-zero is a failure - fall through to the next, then to OSC 52.
fn copy_via_tool(text: &str) -> Option<&'static str> {
    for (name, args) in TOOLS {
        let Ok(mut child) = Command::new(name)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // not on PATH
        };
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
        // Always reap the child before moving on: dropping `Child` does not
        // wait, so a helper that spawned then died (e.g. no display) would
        // otherwise leak a zombie for the mux client's lifetime.
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        if wrote && ok {
            return Some(name);
        }
    }
    None
}

/// Build the OSC 52 clipboard-set escape: `ESC ] 52 ; c ; <base64> BEL`. Caps
/// the input on a char boundary so the base64 stays under the payload cap, and
/// flags truncation.
pub fn osc52_payload(text: &str) -> (Vec<u8>, bool) {
    // 4 base64 chars encode 3 input bytes: the byte budget under the b64 cap.
    const MAX_INPUT: usize = OSC52_B64_CAP / 4 * 3;
    let bytes = text.as_bytes();
    let truncated = bytes.len() > MAX_INPUT;
    let slice = if truncated {
        let mut end = MAX_INPUT;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &bytes[..end]
    } else {
        bytes
    };
    let b64 = base64(slice);
    let mut out = Vec::with_capacity(b64.len() + 8);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(b64.as_bytes());
    out.push(0x07);
    (out, truncated)
}

/// Standard base64 (no line wrap) - the only encoder the crate needs, so a
/// dependency would be overkill.
fn base64(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The first clipboard tool present on `PATH`, or `None` when copy would fall
/// back to the unverifiable OSC 52 path. `fno mux doctor` calls this to flag
/// copy as degraded BEFORE a user hits AC2-ERR in anger (US6 AC6-EDGE). Names
/// come from [`TOOLS`] so the doctor list can never drift from the copy list.
/// Read-only: it inspects PATH entries and never execs a tool ("touches
/// nothing").
pub fn available_tool() -> Option<&'static str> {
    available_tool_with(on_path)
}

/// [`available_tool`] with the PATH predicate injected, so the preference-order
/// and none-present branches are testable without depending on the host's tools.
fn available_tool_with(on_path: impl Fn(&str) -> bool) -> Option<&'static str> {
    TOOLS.iter().map(|(name, _)| *name).find(|name| on_path(name))
}

/// Is `name` a regular file in some `PATH` directory? Pure filesystem lookup, no
/// spawn. ponytail: `is_file` not a full x-bit check - a non-executable name
/// collision on PATH is vanishingly rare and doctor is advisory, not a gate.
fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_tool_none_when_path_empty() {
        // AC6-EDGE: no clipboard tool -> copy is degraded (doctor flags it).
        assert_eq!(available_tool_with(|_| false), None);
    }

    #[test]
    fn available_tool_returns_first_in_preference_order() {
        // Every tool present -> the macOS-first preference order wins.
        assert_eq!(available_tool_with(|_| true), Some("pbcopy"));
    }

    #[test]
    fn available_tool_skips_absent_prefers_present() {
        // Only xclip on PATH: pbcopy/wl-copy skipped, xclip found.
        assert_eq!(available_tool_with(|n| n == "xclip"), Some("xclip"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn osc52_wraps_base64_with_escape_and_bel() {
        let (payload, truncated) = osc52_payload("hi");
        assert!(!truncated);
        assert_eq!(payload.first(), Some(&0x1b));
        assert_eq!(payload.last(), Some(&0x07));
        let mid = std::str::from_utf8(&payload[1..payload.len() - 1]).unwrap();
        assert_eq!(mid, "]52;c;aGk="); // base64("hi") == "aGk="
    }

    #[test]
    fn osc52_caps_long_payload_and_flags_truncation() {
        let big = "x".repeat(1_000_000);
        let (payload, truncated) = osc52_payload(&big);
        assert!(truncated, "over-cap selection flagged");
        // Base64 body stays under the cap (payload = ESC ]52;c; + body + BEL).
        assert!(payload.len() < OSC52_B64_CAP + 16);
    }

    #[test]
    fn local_tool_success_short_circuits_osc52() {
        // AC2-HP: a working local tool lands the copy; no OSC 52 is emitted.
        let mut emitted = false;
        let outcome = deliver_with(
            |_| Some("pbcopy"),
            "copy me",
            |_| {
                emitted = true;
                Ok(())
            },
        );
        assert_eq!(outcome, CopyOutcome::Local("pbcopy"));
        assert!(!emitted, "local success must not also emit OSC 52");
    }

    #[test]
    fn no_tool_falls_back_to_osc52() {
        // No local tool -> the OSC 52 payload is emitted to the terminal.
        let mut captured = Vec::new();
        let outcome = deliver_with(
            |_| None,
            "copy me",
            |bytes| {
                captured.extend_from_slice(bytes);
                Ok(())
            },
        );
        assert_eq!(outcome, CopyOutcome::Osc52 { truncated: false });
        assert!(captured.starts_with(b"\x1b]52;c;"));
    }

    #[test]
    fn both_paths_fail_reports_failure() {
        // AC2-ERR: no tool AND the OSC 52 write errors -> visible failure.
        let outcome = deliver_with(|_| None, "x", |_| Err(std::io::Error::other("blocked")));
        assert_eq!(outcome, CopyOutcome::Failed);
    }
}

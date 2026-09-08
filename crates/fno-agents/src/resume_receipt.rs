//! Resume's preserved-record fallback (x-70e1 task 4): where a resume
//! finds a retired session.

use std::path::Path;

use crate::paths::AgentsHome;

/// The preserved-record fallback (x-70e1 task 4): a session-shaped resume
/// miss consults the reap-receipts store before refusing. A retirement that
/// already removed the registry row keeps the resume tokens and the store
/// context on disk; this prints them (never launching), so the same native
/// session stays reachable without any active fno row. `Some(())` = a
/// receipt matched and was printed.
/// True when a receipt matched and was printed. Kept tiny at the call site:
/// this file is over the shrink-only budget, and its caller needs one bool.
pub(crate) fn maybe_hint_preserved_session(home: &AgentsHome, name: &str) -> bool {
    print_resume_receipt_hint(home, name).is_some()
}

fn print_resume_receipt_hint(home: &AgentsHome, name: &str) -> Option<()> {
    let dir = home.root().join("reap-receipts");
    let entries = std::fs::read_dir(&dir).ok()?;
    let needle = name.trim().to_ascii_lowercase();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(receipt) = crate::receipt::read_reap_receipt(&path) else {
            continue;
        };
        let matches = receipt.harness_session_id.to_ascii_lowercase() == needle
            || receipt.short_id.to_ascii_lowercase() == needle
            || receipt.row_name.to_ascii_lowercase() == needle;
        if !matches {
            continue;
        }
        eprintln!(
            "fno agents resume: {name} has no registry row, but a retirement receipt preserves it."
        );
        eprintln!("  harness: {}", receipt.harness);
        eprintln!("  session: {}", receipt.harness_session_id);
        eprintln!("  original cwd: {}", receipt.cwd);
        if !receipt.cwd.is_empty() && !Path::new(&receipt.cwd).exists() {
            eprintln!(
                "  note: the original cwd is gone; resume from a replacement checkout with the native command below."
            );
        }
        eprintln!(
            "  native resume: {}",
            if receipt.resume_argv.is_empty() {
                receipt.resume.clone()
            } else {
                receipt.resume_argv.join(" ")
            }
        );
        return Some(());
    }
    None
}

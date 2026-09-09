//! Resume's preserved-record fallback (x-70e1 task 4): where a resume
//! finds a retired session.

use std::path::Path;

use crate::paths::AgentsHome;

/// The preserved-record fallback (x-70e1 task 4): a session-shaped resume
/// miss consults the reap-receipts store before refusing. A retirement that
/// already removed the registry row keeps the resume tokens and the store
/// context on disk; this prints [`resume_hint`]'s text (never launching), so
/// the same native session stays reachable without any active fno row.
/// True when a receipt matched and was printed. Kept tiny at the call site:
/// this file is over the shrink-only budget, and its caller needs one bool.
pub(crate) fn maybe_hint_preserved_session(home: &AgentsHome, name: &str) -> bool {
    match resume_hint(home, name) {
        Some(text) => {
            eprint!("{text}");
            true
        }
        None => false,
    }
}

/// Build the preserved-record hint text without printing it: a test can
/// read what a wrapper-failure recovery actually says, rather than only
/// the bool `maybe_hint_preserved_session` prints from.
/// `None` when no receipt matches `name` - nothing to print.
pub fn resume_hint(home: &AgentsHome, name: &str) -> Option<String> {
    let dir = home.root().join("reap-receipts");
    let entries = std::fs::read_dir(&dir).ok()?;
    let needle = name.trim().to_ascii_lowercase();
    // A worker name or short id can be REUSED across retired sessions, so a
    // name match is ambiguous by construction: collect every match, prefer a
    // full-session-id match (unique by construction), and never pick an
    // arbitrary one out of an unspecified directory order.
    let mut matches: Vec<crate::receipt::ReapReceipt> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(receipt) = crate::receipt::read_reap_receipt(&path) else {
            continue;
        };
        let matches_needle = receipt.harness_session_id.to_ascii_lowercase() == needle
            || receipt.short_id.to_ascii_lowercase() == needle
            || receipt.row_name.to_ascii_lowercase() == needle;
        if matches_needle {
            matches.push(receipt);
        }
    }
    if matches.is_empty() {
        return None;
    }
    // Exact session-id matches outrank name matches; there can be at most
    // one, since ids are unique per receipt.
    let exact: Vec<&crate::receipt::ReapReceipt> = matches
        .iter()
        .filter(|r| r.harness_session_id.to_ascii_lowercase() == needle)
        .collect();
    let chosen: Vec<&crate::receipt::ReapReceipt> = if exact.len() == 1 {
        exact
    } else if matches.len() > 1 {
        let mut out = format!(
            "fno agents resume: {name} has no registry row, and {} retirement receipts match it:\n",
            matches.len()
        );
        for receipt in &matches {
            out.push_str(&format!(
                "  {} ({})\n",
                receipt.harness_session_id, receipt.row_name
            ));
        }
        out.push_str("  resume by the exact session id to disambiguate.\n");
        return Some(out);
    } else {
        matches.iter().collect()
    };
    let receipt = chosen[0];
    let mut out = format!(
        "fno agents resume: {name} has no registry row, but a retirement receipt preserves it.\n"
    );
    out.push_str(&format!("  harness: {}\n", receipt.harness));
    out.push_str(&format!("  session: {}\n", receipt.harness_session_id));
    out.push_str(&format!("  original cwd: {}\n", receipt.cwd));
    if !receipt.cwd.is_empty() && !Path::new(&receipt.cwd).exists() {
        out.push_str(
            "  note: the original cwd is gone; resume from a replacement checkout with the native command below.\n",
        );
    }
    out.push_str(&format!(
        "  native resume: {}\n",
        if receipt.resume_argv.is_empty() {
            receipt.resume.clone()
        } else {
            receipt.resume_argv.join(" ")
        }
    ));
    Some(out)
}

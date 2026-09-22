//! Resume's preserved-record fallback (task 4): where a resume
//! finds a retired session.

use std::path::Path;

use crate::paths::AgentsHome;

/// The preserved-record fallback (task 4): a session-shaped resume
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
            "  note: the original cwd is gone; resume from a replacement checkout with the command below.\n",
        );
    }
    // The receipt's own rendered line is the one form a human copies; the
    // deleted branch printed an unquoted argv join that a `[1m]` glob breaks.
    out.push_str(&format!("  resume: {}\n", receipt.resume));
    if receipt.resume_argv.first().map(String::as_str) == Some("fno") {
        out.push_str(
            "  note: this seeds a new session from the transcript on its recorded route; the session id changes.\n",
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with_receipt(receipt: serde_json::Value) -> (tempfile::TempDir, AgentsHome) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agents");
        let dir_path = root.join("reap-receipts");
        std::fs::create_dir_all(&dir_path).unwrap();
        let sid = receipt["harness_session_id"].as_str().unwrap().to_string();
        std::fs::write(
            dir_path.join(format!("claude-{sid}.json")),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        (dir, AgentsHome::at(&root))
    }

    fn receipt_value(sid: &str, resume: &str) -> serde_json::Value {
        serde_json::json!({
            "row_name": "w1",
            "short_id": "w1-id",
            "harness": "claude",
            "harness_session_id": sid,
            "cwd": std::env::temp_dir().to_string_lossy(),
            "created_at": "2026-09-01T00:00:00Z",
            "reaped_at": "2026-09-22T00:00:00Z",
            "resume": resume
        })
    }

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn ac2_hp_zai_door_hint_prints_the_receipt_line_and_the_note() {
        // AC2-HP: the route-door receipt prints its own line and names the
        // session-id change a copied command causes.
        let mut receipt = receipt_value(
            SID,
            "fno agents spawn --resume 11111111-2222-3333-4444-555555555555 -P zai -m 'glm-5.3-flash[1m]'",
        );
        receipt["resume_argv"] = serde_json::json!([
            "fno",
            "agents",
            "spawn",
            "--resume",
            SID,
            "-P",
            "zai",
            "-m",
            "glm-5.3-flash[1m]"
        ]);
        let (_dir, home) = home_with_receipt(receipt);
        let hint = resume_hint(&home, SID).unwrap();
        assert!(
            hint.contains(&format!(
                "  resume: fno agents spawn --resume {SID} -P zai -m 'glm-5.3-flash[1m]'\n"
            )),
            "{hint}"
        );
        assert!(hint.contains("the session id changes"), "{hint}");
    }

    #[test]
    fn ac2_edge_bare_receipt_prints_no_note() {
        // AC2-EDGE: a v1-shaped receipt (no resume_argv) prints the same
        // string it always printed, with no new-session note.
        let (_dir, home) = home_with_receipt(receipt_value(SID, &format!("claude --resume {SID}")));
        let hint = resume_hint(&home, SID).unwrap();
        assert!(
            hint.contains(&format!("  resume: claude --resume {SID}\n")),
            "{hint}"
        );
        assert!(!hint.contains("the session id changes"), "{hint}");
    }
}

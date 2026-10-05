//! Which of a repo's lead evals still owe their Part 4 reforms: the
//! question the check-in beat answers so a successor files what a
//! predecessor left open.

use std::path::{Path, PathBuf};

/// The repo's unfilled eval part4 files: `status: pending` under
/// `<plans>/../evals/kings/*/part4-reforms.md`. The eval writer stamps
/// pending and the lead flips the status when the reforms are filed, so a
/// beat can name what a predecessor left open. Best effort: an unreadable
/// eval tree names nothing.
pub fn unfilled_part4s(cwd: &Path) -> Vec<PathBuf> {
    let Some(plans) = crate::plans_path::plans_content_dir(cwd) else {
        return Vec::new();
    };
    let kings = plans.join("..").join("evals").join("kings");
    let Ok(entries) = std::fs::read_dir(&kings) else {
        return Vec::new();
    };
    let mut unfilled = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join("part4-reforms.md");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // get, not slice: byte 64 can land inside a multibyte character,
        // and a panic here would take the whole check-in beat down.
        let head = text.get(..64).unwrap_or(&text);
        if head.contains("status: pending") {
            unfilled.push(path);
        }
    }
    unfilled.sort();
    unfilled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfilled_part4s_names_pending_and_skips_done() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("proj");
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        let plans = base.path().join("plans");
        std::fs::write(
            repo.join(".claude/settings.local.json"),
            format!(r#"{{"plansDirectory": "{}"}}"#, plans.display()),
        )
        .unwrap();
        let kings = plans.join("evals").join("kings");
        std::fs::create_dir_all(kings.join("lead-a-11111111")).unwrap();
        std::fs::create_dir_all(kings.join("lead-b-22222222")).unwrap();
        std::fs::create_dir_all(kings.join("lead-c-33333333")).unwrap();
        std::fs::write(
            kings.join("lead-a-11111111/part4-reforms.md"),
            "---\nstatus: pending\n---\n\n# Part 4: reforms\n",
        )
        .unwrap();
        std::fs::write(
            kings.join("lead-b-22222222/part4-reforms.md"),
            "---\nstatus: done\n---\n\n# Part 4: reforms\n",
        )
        .unwrap();
        // A multibyte character straddles byte 64 (27 header bytes + 36
        // ASCII): reading the head must not panic and must still find the
        // status line.
        let straddle = format!("---\nstatus: pending\n---\n\n# {}部after\n", "a".repeat(36));
        std::fs::write(kings.join("lead-c-33333333/part4-reforms.md"), straddle).unwrap();
        assert_eq!(
            unfilled_part4s(&repo),
            vec![
                kings.join("lead-a-11111111/part4-reforms.md"),
                kings.join("lead-c-33333333/part4-reforms.md"),
            ]
        );
    }
}

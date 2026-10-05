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
        let head = &text[..text.len().min(64)];
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
        assert_eq!(
            unfilled_part4s(&repo),
            vec![kings.join("lead-a-11111111/part4-reforms.md")]
        );
    }
}

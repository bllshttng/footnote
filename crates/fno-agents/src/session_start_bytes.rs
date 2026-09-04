//! `fno-agents session-start-bytes --gate-bytes N [--cwd PATH]` -- the
//! operator's TOTAL session-start byte count (x-997a), so preamble overload
//! is a number, not a feeling.
//!
//! `--gate-bytes` is required rather than re-derived: `check-preamble-budget.sh`
//! already computes the fno gate figure (A) and this verb must not carry a
//! second implementation of that parse. This verb owns the other two pieces:
//! the user's global `~/.claude/CLAUDE.md` plus every file its top-level `@`
//! lines import (B), and the current project's memory index (C,
//! `~/.claude/projects/<slug>/memory/MEMORY.md`, the same slug
//! [`crate::client_verbs::claude_cwd_slug`] resolves for the transcript
//! store). Best-effort: a missing piece counts as zero bytes.

use crate::client_verbs::claude_cwd_slug;
use std::path::PathBuf;

fn user_claude_md_bytes(home: &std::path::Path) -> u64 {
    let claude_md = home.join(".claude").join("CLAUDE.md");
    let Ok(text) = std::fs::read_to_string(&claude_md) else {
        return 0;
    };
    let mut total = text.len() as u64;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('@') else {
            continue;
        };
        let name = rest.split_whitespace().next().unwrap_or(rest);
        if let Ok(meta) = std::fs::metadata(home.join(".claude").join(name)) {
            total += meta.len();
        }
    }
    total
}

/// `projects_dir` is the resolved Claude projects base (not read from the
/// environment here, so a test can point it at a fixture without mutating
/// process-wide state another test module's own tests read concurrently).
fn memory_index_bytes(projects_dir: &std::path::Path, cwd: &std::path::Path) -> u64 {
    let index = projects_dir
        .join(claude_cwd_slug(cwd))
        .join("memory")
        .join("MEMORY.md");
    std::fs::metadata(index).map(|m| m.len()).unwrap_or(0)
}

pub fn run_session_start_bytes(args: &[String]) -> i32 {
    let mut gate_bytes: Option<u64> = None;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gate-bytes" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) => gate_bytes = Some(v),
                    None => {
                        eprintln!(
                            "fno-agents session-start-bytes: --gate-bytes needs a non-negative integer"
                        );
                        return 2;
                    }
                }
            }
            "--cwd" => {
                i += 1;
                match args.get(i) {
                    Some(p) => cwd = PathBuf::from(p),
                    None => {
                        eprintln!("fno-agents session-start-bytes: --cwd needs a path");
                        return 2;
                    }
                }
            }
            other => {
                eprintln!("fno-agents session-start-bytes: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(gate_bytes) = gate_bytes else {
        eprintln!("fno-agents session-start-bytes: --gate-bytes is required");
        return 2;
    };

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let user_bytes = user_claude_md_bytes(&home);
    let memory_bytes = memory_index_bytes(&crate::claude_drive::claude_projects_dir(), &cwd);
    let total = gate_bytes + user_bytes + memory_bytes;

    println!(
        "preamble: fno gate {gate_bytes} B + user CLAUDE.md and imports {user_bytes} B + \
         project memory index {memory_bytes} B = {total} B (~{} tok per session start)",
        total / 4
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sums_the_user_claude_md_and_its_import() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(home.path().join(".claude/CLAUDE.md"), "abc\n@RTK.md\n").unwrap();
        std::fs::write(home.path().join(".claude/RTK.md"), "wxyz").unwrap();
        assert_eq!(user_claude_md_bytes(home.path()), 16); // 12 (CLAUDE.md) + 4 (RTK.md)
    }

    #[test]
    fn reads_the_memory_index_at_its_slugged_path() {
        let projects_dir = tempfile::tempdir().expect("tempdir");
        let cwd = PathBuf::from("/fixture/project");
        let dir = projects_dir
            .path()
            .join(claude_cwd_slug(&cwd))
            .join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join("MEMORY.md")).unwrap();
        write!(f, "12345").unwrap();
        assert_eq!(memory_index_bytes(projects_dir.path(), &cwd), 5);
    }

    #[test]
    fn a_missing_memory_index_counts_as_zero() {
        let projects_dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            memory_index_bytes(projects_dir.path(), &PathBuf::from("/nowhere")),
            0
        );
    }

    #[test]
    fn a_missing_claude_md_counts_as_zero() {
        let home = tempfile::tempdir().expect("tempdir");
        assert_eq!(user_claude_md_bytes(home.path()), 0);
    }

    #[test]
    fn missing_gate_bytes_is_a_usage_error() {
        assert_eq!(run_session_start_bytes(&[]), 2);
    }
}

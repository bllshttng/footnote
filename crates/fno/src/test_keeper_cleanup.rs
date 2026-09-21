use std::path::Path;
use std::time::{Duration, Instant};

fn names_test_dir(command: &str, root: &Path) -> bool {
    let root_prefix = format!("{}/", root.display());
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| matches!(pair[0], "--sock" | "--cwd") && pair[1].starts_with(&root_prefix))
}

/// The keeper pids naming this test directory, plus the pids of the pane
/// children those keepers host. The children ride along because killing only
/// a keeper leaves a hosted child that ignores SIGHUP alive as an untracked
/// orphan.
fn keeper_and_child_pids(root: &Path) -> Option<(Vec<i32>, Vec<i32>)> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    struct Row {
        pid: i32,
        ppid: i32,
        keeper: bool,
    }
    let mut rows = Vec::new();
    let mut keepers = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.trim_start().splitn(3, char::is_whitespace);
        let Some(pid) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let Some(ppid) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let command = fields.next().unwrap_or("");
        let keeper = pid != std::process::id() as i32
            && command.contains("fno-agents-worker")
            && command.contains("--pane")
            && names_test_dir(command, root);
        if keeper {
            keepers.push(pid);
        }
        rows.push(Row { pid, ppid, keeper });
    }
    keepers.sort_unstable();
    keepers.dedup();
    let mut children: Vec<i32> = rows
        .iter()
        .filter(|row| !row.keeper && keepers.contains(&row.ppid))
        .map(|row| row.pid)
        .collect();
    children.sort_unstable();
    children.dedup();
    Some((keepers, children))
}

pub(crate) fn reap(root: &Path) {
    let Some((keepers, children)) = keeper_and_child_pids(root) else {
        return;
    };
    for pid in children.iter().chain(keepers.iter()) {
        // SAFETY: each pid is this test directory's fno-agents-worker --pane
        // command line or its direct hosted child; SIGKILL bounds teardown.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match keeper_and_child_pids(root) {
            Some((keepers, children)) if keepers.is_empty() && children.is_empty() => return,
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
            None => return,
        }
    }
    if let Some((keepers, children)) = keeper_and_child_pids(root) {
        if !keepers.is_empty() || !children.is_empty() {
            eprintln!(
                "fno mux test cleanup: keepers or hosted children still name {}: keepers={keepers:?} children={children:?}",
                root.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::names_test_dir;
    use std::path::Path;

    #[test]
    fn test_directory_match_does_not_accept_a_thread_id_prefix() {
        let root = Path::new("/tmp/fno-mux-test-123-ThreadId(2)");
        assert!(names_test_dir(
            "fno-agents-worker --pane --sock /tmp/fno-mux-test-123-ThreadId(2)/panes/test-1.sock --session test",
            root
        ));
        assert!(!names_test_dir(
            "fno-agents-worker --pane --sock /tmp/fno-mux-test-123-ThreadId(20)/panes/test-1.sock --session test",
            root
        ));
        assert!(!names_test_dir(
            "fno-agents-worker --pane --cwd /tmp/fno-mux-test-123-ThreadId(20)/repo --session test",
            root
        ));
    }
}

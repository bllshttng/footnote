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

fn keeper_pids(root: &Path) -> Option<Vec<i32>> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.trim_start().splitn(2, char::is_whitespace);
        let Some(pid) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let command = fields.next().unwrap_or("");
        if pid != std::process::id() as i32
            && command.contains("fno-agents-worker")
            && command.contains("--pane")
            && names_test_dir(command, root)
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Some(pids)
}

pub(crate) fn reap(root: &Path) {
    let Some(pids) = keeper_pids(root) else {
        return;
    };
    for pid in pids {
        // SAFETY: keeper_pids matched this pid to this test directory's
        // fno-agents-worker --pane command line; SIGKILL bounds teardown.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match keeper_pids(root) {
            Some(pids) if pids.is_empty() => return,
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
            None => return,
        }
    }
    if let Some(pids) = keeper_pids(root) {
        if !pids.is_empty() {
            eprintln!(
                "fno mux test cleanup: pane keepers still name {}: {pids:?}",
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

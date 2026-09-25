//! Branch-local test-count changes for the PR description.

use std::process::Command;

#[derive(Default, Debug, PartialEq, Eq)]
struct LanguageDelta {
    added: u64,
    removed: u64,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct TestDelta {
    python: LanguageDelta,
    rust: LanguageDelta,
    shell: LanguageDelta,
}

impl TestDelta {
    fn from_diff(diff: &str) -> Self {
        let mut result = Self::default();
        for line in diff.lines() {
            let (added, source) = match line.as_bytes().first() {
                Some(b'+') if !line.starts_with("+++") => (true, &line[1..]),
                Some(b'-') if !line.starts_with("---") => (false, &line[1..]),
                _ => continue,
            };
            let code = source.trim_start();
            let delta = if code.starts_with("def test_") || code.starts_with("async def test_") {
                Some(&mut result.python)
            } else if code.trim() == "#[test]" || code.trim() == "#[tokio::test]" {
                Some(&mut result.rust)
            } else if code.starts_with("@test ")
                || code.starts_with("test_") && code.contains("() {")
            {
                Some(&mut result.shell)
            } else {
                None
            };
            if let Some(delta) = delta {
                if added {
                    delta.added += 1;
                } else {
                    delta.removed += 1;
                }
            }
        }
        result
    }

    fn markdown(&self) -> String {
        let added = self.python.added + self.rust.added + self.shell.added;
        let removed = self.python.removed + self.rust.removed + self.shell.removed;
        format!(
            "| Language | Added | Removed | Net |\n|---|---:|---:|---:|\n| Python | {} | {} | {} |\n| Rust | {} | {} | {} |\n| Shell | {} | {} | {} |\n| **Total** | **{}** | **{}** | **{:+}** |",
            self.python.added,
            self.python.removed,
            net(&self.python),
            self.rust.added,
            self.rust.removed,
            net(&self.rust),
            self.shell.added,
            self.shell.removed,
            net(&self.shell),
            added,
            removed,
            added as i64 - removed as i64,
        )
    }
}

fn net(delta: &LanguageDelta) -> i64 {
    delta.added as i64 - delta.removed as i64
}

/// `fno-agents test-delta --base <ref>` prints test declaration changes on
/// the current branch. Python test functions, Rust test attributes, and shell
/// test declarations are counted from the committed merge-base diff.
pub fn run_test_delta(args: &[String]) -> i32 {
    let Some(base_pos) = args.iter().position(|arg| arg == "--base") else {
        eprintln!("usage: fno-agents test-delta --base <ref>");
        return 2;
    };
    let Some(base) = args.get(base_pos + 1).filter(|base| !base.starts_with('-')) else {
        eprintln!("test-delta: --base requires a ref");
        return 2;
    };
    let range = format!("{base}...HEAD");
    let output = match Command::new("git")
        .args([
            "diff",
            "--unified=0",
            range.as_str(),
            "--",
            "*.py",
            "*.rs",
            "*.sh",
        ])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!(
                "test-delta: git diff failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return 1;
        }
        Err(err) => {
            eprintln!("test-delta: could not run git diff: {err}");
            return 1;
        }
    };
    let delta = TestDelta::from_diff(&String::from_utf8_lossy(&output.stdout));
    println!("{}", delta.markdown());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_added_removed_and_net_test_declarations_by_language() {
        let diff = concat!(
            "+def test_new_case():\n",
            "+#[test]\n",
            "+#[tokio::test]\n",
            "+@test \"shell case\" {\n",
            "-def test_old_case():\n",
            "-#[test]\n",
            "-test_old_shell() {\n",
            "+not_a_test() {\n",
            "+++ b/tests/test_added.py\n",
            "--- a/tests/test_removed.py\n",
        );
        let delta = TestDelta::from_diff(diff);
        assert_eq!(
            delta.python,
            LanguageDelta {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(
            delta.rust,
            LanguageDelta {
                added: 2,
                removed: 1
            }
        );
        assert_eq!(
            delta.shell,
            LanguageDelta {
                added: 1,
                removed: 1
            }
        );
        assert!(delta
            .markdown()
            .contains("| **Total** | **4** | **3** | **+1** |"));
    }
}

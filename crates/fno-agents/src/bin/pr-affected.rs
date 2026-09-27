use std::process::Command;

#[derive(Debug, PartialEq, Eq)]
struct Selection {
    cargo: bool,
    python_full: bool,
}

fn select_jobs(event: &str, paths: Option<&[String]>, packet_fits: Option<bool>) -> Selection {
    if event != "pull_request" {
        return Selection {
            cargo: true,
            python_full: true,
        };
    }

    let Some(paths) = paths.filter(|paths| !paths.is_empty()) else {
        return Selection {
            cargo: true,
            python_full: true,
        };
    };

    let cargo = paths.iter().any(|path| {
        path == "docs/harnesses/capability-matrix.md"
            || path == "docs/architecture/product-boundaries.md"
            || !(path.starts_with("cli/tests/")
                || path.starts_with("skills/")
                || path.starts_with("agents/")
                || path.starts_with(".claude-plugin/")
                || path.starts_with("docs/"))
    });
    let python_full = packet_fits != Some(true)
        || paths
            .iter()
            .any(|path| path == ".github/workflows/cli-ci.yml");
    Selection { cargo, python_full }
}

fn changed_paths(base: &str, head: &str) -> Option<Vec<String>> {
    let range = format!("{base}...{head}");
    let output = Command::new("git")
        .args(["diff", "--name-only", "--no-renames", &range])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let paths: Vec<String> = String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .map(str::to_owned)
        .collect();
    (!paths.is_empty()).then_some(paths)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let event = args.first().map(String::as_str).unwrap_or("");
    let selection = if event == "pull_request" {
        let paths = args
            .get(1)
            .zip(args.get(2))
            .and_then(|(base, head)| changed_paths(base, head));
        let packet_fits = args.get(3).map(|value| value == "true");
        select_jobs(event, paths.as_deref(), packet_fits)
    } else {
        select_jobs(event, None, None)
    };
    println!("cargo={}", selection.cargo);
    println!("python_full={}", selection.python_full);
}

#[cfg(test)]
mod tests {
    use super::{select_jobs, Selection};

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn docs_only_pr_skips_cargo_and_full_python_when_packet_fits() {
        let changed = paths(&["docs/usage.md", "cli/tests/unit/test_example.py"]);
        assert_eq!(
            select_jobs("pull_request", Some(&changed), Some(true)),
            Selection {
                cargo: false,
                python_full: false,
            }
        );
    }

    #[test]
    fn cargo_fence_paths_and_docs_exceptions_keep_cargo_on() {
        for changed in [
            "hooks/target-stop-hook.sh",
            "schemas/event.json",
            "scripts/ci/check.sh",
            "generated-artifacts.tsv",
            "skill-bundles.yaml",
            ".github/workflows/cli-ci.yml",
            "crates/fno-agents/src/lib.rs",
            "cli/src/fno/test_cmd.py",
            "docs/harnesses/capability-matrix.md",
            "docs/architecture/product-boundaries.md",
        ] {
            let changed = paths(&[changed]);
            assert!(
                select_jobs("pull_request", Some(&changed), Some(true)).cargo,
                "cargo must run for {changed:?}"
            );
        }
    }

    #[test]
    fn an_unfit_packet_keeps_full_python_on() {
        let changed = paths(&["docs/usage.md"]);
        assert_eq!(
            select_jobs("pull_request", Some(&changed), Some(false)),
            Selection {
                cargo: false,
                python_full: true,
            }
        );
    }

    #[test]
    fn missing_or_empty_diff_fails_closed() {
        assert_eq!(
            select_jobs("pull_request", None, Some(true)),
            Selection {
                cargo: true,
                python_full: true,
            }
        );
        let empty = paths(&[]);
        assert_eq!(
            select_jobs("pull_request", Some(&empty), Some(true)),
            Selection {
                cargo: true,
                python_full: true,
            }
        );
        assert_eq!(
            select_jobs("pull_request", Some(&paths(&["docs/usage.md"])), None),
            Selection {
                cargo: false,
                python_full: true,
            }
        );
    }

    #[test]
    fn non_pr_events_always_run_both_lanes() {
        for event in ["push", "schedule", "workflow_dispatch"] {
            assert_eq!(
                select_jobs(event, None, None),
                Selection {
                    cargo: true,
                    python_full: true,
                }
            );
        }
    }

    #[test]
    fn bad_git_revision_fails_closed() {
        let Some(paths) = super::changed_paths("missing-pr-base", "missing-pr-head") else {
            assert_eq!(
                select_jobs("pull_request", None, Some(true)),
                Selection {
                    cargo: true,
                    python_full: true,
                }
            );
            return;
        };
        assert!(!paths.is_empty());
        panic!("an invalid revision range unexpectedly produced changed paths");
    }
}

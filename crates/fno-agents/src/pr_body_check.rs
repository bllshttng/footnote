//! `fno-agents pr-body-check` -- run the repo's own PR-body guards against a
//! composed body file BEFORE `gh pr create` opens the PR.
//!
//! Every body guard is a pure function of the body text, the title and the
//! branch name, yet CI is where a bad body was first discovered: a red job
//! costs a full workflow round, a body edit and a pushed commit, and reads
//! indistinguishably from a code failure. This verb runs the same scripts CI
//! runs -- it holds no second copy of any rule -- so first detection moves to
//! authoring time. The CI guards stay as the backstop.
//!
//! Exit codes:
//! * `0` every guard passed or was skipped (not in this repo)
//! * `1` at least one guard failed
//! * `2` usage or read error (no body file, not a git repo, merge base
//!   unresolvable, guard timeout) -- never a body verdict

use std::path::{Path, PathBuf};
use std::time::Duration;

/// One guard run is a local script over a body string; 60s matches the
/// pr-push read bound and is generous next to the sub-second these take.
const GUARD_TIMEOUT: Duration = Duration::from_secs(60);

/// The body-only guards, in CI's own order, resolved under the repo root so
/// the run sees the copy CI runs on this branch. Env contract, identical for
/// every guard: `PR_BODY`, `PR_TITLE`, `PR_HEAD_REF`, `PR_HEAD_SHA=HEAD`,
/// `PR_BASE_SHA` (merge-base of `origin/<base>` and HEAD). A script missing
/// from this repo is a `skip`, not a failure, so a repo carrying none of
/// these guards passes vacuously.
const GUARDS: [&str; 3] = [
    "scripts/ci/check-no-session-urls.sh",
    "scripts/ci/check-pr-node-closure.sh",
    "scripts/ci/check-oos-tracked.sh",
];

const USAGE: &str = "usage: fno-agents pr-body-check --body-file <path|-> [--title <t>] [--head <ref>] [--base <branch>]";

#[derive(Debug)]
struct Args {
    body_file: String,
    title: String,
    head: Option<String>,
    base: String,
    cwd: PathBuf,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        body_file: String::new(),
        title: String::new(),
        head: None,
        base: "main".to_string(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let mut i = 0;
    while i < argv.len() {
        let take = |name: &str| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match argv[i].as_str() {
            "--body-file" => {
                a.body_file = take("--body-file")?;
                i += 1;
            }
            "--title" => {
                a.title = take("--title")?;
                i += 1;
            }
            "--head" => {
                a.head = Some(take("--head")?);
                i += 1;
            }
            "--base" => {
                a.base = take("--base")?;
                i += 1;
            }
            // Test seam, same as pr-push's.
            "--cwd" => {
                a.cwd = PathBuf::from(take("--cwd")?);
                i += 1;
            }
            other => return Err(format!("unknown flag: {other}\n{USAGE}")),
        }
        i += 1;
    }
    if a.body_file.is_empty() {
        return Err(format!("--body-file is required\n{USAGE}"));
    }
    Ok(a)
}

/// `-` reads stdin; anything else is a filesystem path. An unreadable path
/// is a read error that names the path, never a body verdict.
fn read_body(spec: &str) -> Result<String, String> {
    if spec == "-" {
        use std::io::Read;
        let mut body = String::new();
        std::io::stdin()
            .read_to_string(&mut body)
            .map_err(|e| format!("could not read the body from stdin: {e}"))?;
        Ok(body)
    } else {
        std::fs::read_to_string(spec).map_err(|_| format!("body file not found: {spec}"))
    }
}

fn git(git_bin: &str, cwd: &Path, args: &[&str]) -> Result<String, String> {
    let (ok, out, err) =
        crate::pr_push::run_labeled("pr-body-check", git_bin, args, cwd, GUARD_TIMEOUT)?;
    if ok {
        Ok(out.trim().to_string())
    } else {
        Err(if err.trim().is_empty() {
            out.trim().to_string()
        } else {
            err.trim().to_string()
        })
    }
}

pub fn run(argv: &[String]) -> i32 {
    let a = match parse_args(argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("pr-body-check: {msg}");
            return 2;
        }
    };
    let body = match read_body(&a.body_file) {
        Ok(b) => b,
        Err(msg) => {
            eprintln!("pr-body-check: {msg}");
            return 2;
        }
    };
    let git_bin = "git";
    let root = match git(git_bin, &a.cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(r) if !r.is_empty() => PathBuf::from(r),
        _ => {
            eprintln!(
                "pr-body-check: not a git repository (cwd {}); run it from the checkout the PR opens from",
                a.cwd.display()
            );
            return 2;
        }
    };
    let head = match &a.head {
        Some(h) => h.clone(),
        None => match git(git_bin, &a.cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            Ok(h) if !h.is_empty() => h,
            _ => {
                eprintln!(
                    "pr-body-check: could not read the head ref; pass --head <ref> (detached HEAD?)"
                );
                return 2;
            }
        },
    };
    let base_ref = format!("origin/{}", a.base);
    let base_sha = match git(git_bin, &a.cwd, &["merge-base", &base_ref, "HEAD"]) {
        Ok(sha) if !sha.is_empty() => sha,
        _ => {
            eprintln!(
                "pr-body-check: could not resolve git merge-base {base_ref} HEAD; run git fetch origin {} and retry",
                a.base
            );
            return 2;
        }
    };

    // The guard child inherits this process's environment, so the shared env
    // contract is stamped once before the loop. The verb is single-threaded:
    // no reader can see a torn write.
    std::env::set_var("PR_BODY", &body);
    std::env::set_var("PR_TITLE", &a.title);
    std::env::set_var("PR_HEAD_REF", &head);
    std::env::set_var("PR_HEAD_SHA", "HEAD");
    std::env::set_var("PR_BASE_SHA", &base_sha);

    let mut ran = 0usize;
    let mut failed = 0usize;
    for guard in GUARDS {
        let name = guard.rsplit('/').next().unwrap_or(guard);
        let script = root.join(guard);
        if !script.is_file() {
            println!("skip {name}: not in this repo");
            continue;
        }
        let script_str = script.to_string_lossy().into_owned();
        match crate::pr_push::run_labeled(
            "pr-body-check",
            "bash",
            &[script_str.as_str()],
            &root,
            GUARD_TIMEOUT,
        ) {
            Ok((true, _, _)) => {
                ran += 1;
                println!("pass {name}");
            }
            Ok((false, out, err)) => {
                ran += 1;
                failed += 1;
                println!("fail {name}");
                // The guard's own report, unchanged: stdout to stdout, stderr
                // to stderr.
                print!("{out}");
                eprint!("{err}");
            }
            Err(e) => {
                // A hung guard is a broken environment, not a body verdict:
                // read-error class, exit 2. run_labeled already formatted the
                // bounded-transport diagnostic.
                eprintln!("{e}");
                return 2;
            }
        }
    }

    println!("pr-body-check: {ran} ran, {failed} failed");
    if failed > 0 {
        eprintln!("the PR body is not a commit: fix the body file, rerun this verb, then create or edit the PR");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn body_file_is_required() {
        let err = parse_args(&argv(&[])).unwrap_err();
        assert!(err.contains("--body-file is required"), "{err}");
        assert!(err.contains("usage: fno-agents pr-body-check"), "{err}");
    }

    #[test]
    fn defaults_are_base_main_and_cwd() {
        let a = parse_args(&argv(&["--body-file", "b.md"])).unwrap();
        assert_eq!(a.body_file, "b.md");
        assert_eq!(a.base, "main");
        assert_eq!(a.title, "");
        assert!(a.head.is_none());
    }

    #[test]
    fn every_flag_takes_its_value() {
        let a = parse_args(&argv(&[
            "--body-file",
            "-",
            "--title",
            "t",
            "--head",
            "feature/body-check",
            "--base",
            "develop",
        ]))
        .unwrap();
        assert_eq!(a.body_file, "-");
        assert_eq!(a.title, "t");
        assert_eq!(a.head.as_deref(), Some("feature/body-check"));
        assert_eq!(a.base, "develop");
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        let err = parse_args(&argv(&["--body-file", "b.md", "--nope"])).unwrap_err();
        assert!(err.contains("unknown flag: --nope"), "{err}");
    }

    #[test]
    fn a_missing_body_file_names_the_path() {
        let err = read_body("/nonexistent/pr-body.md").unwrap_err();
        assert!(err.contains("/nonexistent/pr-body.md"), "{err}");
    }
}

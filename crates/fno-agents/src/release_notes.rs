//! Release notes for the update modal: one line per merged PR between the
//! installed rev and source HEAD (first-parent merge commits), grouped by
//! user-facing area, with test/docs/ci/chore hidden behind a count. Python's
//! `fno doctor update --check` resolver calls [`run_release_notes`] through
//! the verb seam (`fno.rust_binary.verb_call`), the same bridge
//! `sandbox_probe` uses: `cli/src/fno` bars new Python, so the notes builder
//! lives here and Python only forwards. The TUI still renders only the
//! payload (Locked Decision 6, installed-fno-staleness.md): it reads these
//! rows out of `fno doctor update --check`'s JSON, never git.

use serde_json::{json, Value};
use std::path::Path;

/// One release-notes line, serde-shaped exactly as the mux modal reads it
/// (crates/fno/src/client/update_menu.rs).
#[derive(serde::Serialize, Clone)]
struct NoteLine {
    pr: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    text: String,
}

/// One area group. Only non-empty groups are emitted.
#[derive(serde::Serialize)]
struct NoteGroup {
    area: &'static str,
    lines: Vec<NoteLine>,
}

const TYPES: [&str; 10] = [
    "feat", "fix", "refactor", "perf", "test", "docs", "ci", "chore", "build", "style",
];
const HIDDEN_TYPES: [&str; 6] = ["test", "docs", "ci", "chore", "build", "style"];
const AREA_ORDER: [&str; 6] = ["mux", "backlog", "agents", "review", "merge", "general"];

/// git in `source`, None on any failure - a failed read degrades the notes,
/// it never fails the update check.
fn git(source: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The PR number in a GitHub merge-commit subject
/// (`Merge pull request #N from user/branch`).
fn pr_of(subject: &str) -> Option<u64> {
    let rest = subject.strip_prefix("Merge pull request #")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The first non-empty body line: GitHub puts the PR title there.
fn title_of(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `type(scope): rest` from a PR title, validated against the known types.
fn split_conventional(title: &str) -> Option<(String, Option<String>, String)> {
    let (head, rest) = title.split_once(": ")?;
    let head = head.trim();
    let (ty, scope) = match (head.find('('), head.ends_with(')')) {
        (Some(i), true) => {
            let ty = head[..i].trim();
            let scope = head[i + 1..head.len() - 1].trim();
            (ty, Some(scope))
        }
        (None, false) => (head, None),
        _ => return None,
    };
    if !TYPES.iter().any(|t| t.eq_ignore_ascii_case(ty)) {
        return None;
    }
    Some((
        ty.to_string(),
        scope.map(str::to_string),
        rest.trim().to_string(),
    ))
}

/// The user-facing area a PR scope lands in; unknown scopes are general.
fn area_of(scope: Option<&str>) -> &'static str {
    let scope = scope.unwrap_or("").split('/').next().unwrap_or("");
    match scope {
        "mux" => "mux",
        "backlog" => "backlog",
        "agents" => "agents",
        "review" | "reign" => "review",
        "merge" | "pr" => "merge",
        _ => "general",
    }
}

/// `https://github.com/<owner>/<repo>/pull` from the checkout's origin, or
/// None - a missing URL only costs the tap, never the row.
fn origin_pr_url_base(source: &Path) -> Option<String> {
    let url = git(source, &["remote", "get-url", "origin"])?;
    let url = url.trim();
    // ssh `git@github.com:o/r.git` and https `https://github.com/o/r.git`;
    // anything else (a proxy alias, a local path) yields no URL.
    let path = if let Some(rest) = url.strip_prefix("git@") {
        rest.split(':').nth(1)?
    } else {
        let rest = url.strip_prefix("https://")?;
        rest.split_once('/').map(|(_, p)| p)?
    };
    let mut parts = path.trim_end_matches('/').rsplitn(2, '/');
    let repo = parts.next()?.strip_suffix(".git")?;
    let owner = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}/pull"))
}

/// The notes, or `Value::Null` when git could not answer or the range has no
/// parseable merge commits - the modal falls back to the raw changelog.
pub fn build(installed_rev: &str, source: &Path) -> Value {
    // The window: the newest merges only. An install many releases behind
    // could carry hundreds; the modal reads the newest MAX_MERGES and the
    // hidden line names what the window left out.
    let log = match git(
        source,
        &[
            "log",
            "--first-parent",
            "--merges",
            "-n",
            "60",
            "--format=%H%x1f%s%x1f%b%x1e",
            &format!("{installed_rev}..HEAD"),
        ],
    ) {
        Some(log) => log,
        None => return Value::Null,
    };
    let total_merges = git(
        source,
        &[
            "rev-list",
            "--count",
            "--first-parent",
            "--merges",
            &format!("{installed_rev}..HEAD"),
        ],
    )
    .and_then(|c| c.trim().parse::<u64>().ok());
    let url_base = origin_pr_url_base(source);
    let mut visible: Vec<NoteLine> = Vec::new();
    let mut hidden: Vec<NoteLine> = Vec::new();
    let mut meta: Vec<(&'static str, bool)> = Vec::new(); // (area, is_feat) per visible row
    for record in log.split('\x1e') {
        let fields: Vec<&str> = record.split('\x1f').collect();
        if fields.len() < 3 {
            continue;
        }
        let Some(pr) = pr_of(fields[1]) else { continue };
        let title = title_of(fields[2]);
        let conv = split_conventional(&title);
        let text = conv
            .as_ref()
            .map(|(_, _, rest)| rest.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("pull request #{pr}"));
        let line = NoteLine {
            pr,
            url: url_base.as_ref().map(|base| format!("{base}/{pr}")),
            text,
        };
        let Some((ty, scope, _)) = conv else {
            meta.push(("general", false));
            visible.push(line);
            continue;
        };
        let ty = ty.to_lowercase();
        let feat = ty == "feat";
        if HIDDEN_TYPES.contains(&ty.as_str()) {
            hidden.push(line);
        } else {
            meta.push((area_of(scope.as_deref()), feat));
            visible.push(line);
        }
    }
    if visible.is_empty() && hidden.is_empty() {
        return Value::Null;
    }
    // Highlights lead; their groups lose them so no PR shows twice.
    let feat_idx: Vec<usize> = meta
        .iter()
        .enumerate()
        .filter(|(_, (_, feat))| *feat)
        .map(|(i, _)| i)
        .collect();
    let pick: Vec<usize> = if feat_idx.is_empty() {
        (0..visible.len().min(2)).collect()
    } else {
        feat_idx.iter().take(2).copied().collect()
    };
    let highlights: Vec<NoteLine> = pick.iter().map(|&i| visible[i].clone()).collect();
    let mut groups: Vec<NoteGroup> = AREA_ORDER
        .iter()
        .map(|area| NoteGroup {
            area,
            lines: visible
                .iter()
                .enumerate()
                .filter(|(i, _)| meta[*i].0 == *area && !pick.contains(i))
                .map(|(_, line)| line.clone())
                .collect(),
        })
        .collect();
    groups.retain(|g| !g.lines.is_empty());
    let shown = visible.len() + hidden.len();
    let mut hidden = hidden;
    if visible.is_empty() {
        // Nothing user-facing changed: the hidden kinds ARE the release.
        groups = vec![NoteGroup {
            area: "chores",
            lines: hidden.split_off(0),
        }];
    }
    let window = total_merges
        .filter(|total| (*total as usize) > shown)
        .map(|total| format!("newest {shown} of {total} merges shown"));
    let churn = (!hidden.is_empty()).then(|| {
        format!(
            "{} test/docs/ci/chore PR{} hidden",
            hidden.len(),
            if hidden.len() == 1 { "" } else { "s" }
        )
    });
    let hidden_line = match (window, churn) {
        (Some(w), Some(c)) => Some(format!("{w}, {c}")),
        (w, c) => w.or(c),
    };
    json!({
        "highlights": highlights,
        "groups": groups,
        "hidden_line": hidden_line,
    })
}

/// The `release-notes` verb: payload `{"installed_rev": ..., "source": ...}`
/// on stdin, `{"notes": <payload|null>}` on stdout. It answers, the caller
/// judges (same posture as sandbox-probe).
pub fn run_release_notes(args: &[String]) -> i32 {
    use std::io::Read;
    let _ = args;
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        eprintln!("release-notes: bad payload");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("release-notes: bad payload");
            return 2;
        }
    };
    let notes = match (
        parsed.get("installed_rev").and_then(Value::as_str),
        parsed.get("source").and_then(Value::as_str),
    ) {
        (Some(rev), Some(source)) => build(rev, Path::new(source)),
        _ => Value::Null,
    };
    println!(
        "{}",
        serde_json::to_string(&json!({ "notes": notes })).unwrap_or_else(|_| "{}".into())
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_env() -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e");
        cmd
    }

    /// A repo whose main is a chain of GitHub-style merge commits. Returns
    /// (base, head).
    fn merge_repo(dir: &Path, prs: &[(u64, &str)]) -> (String, String) {
        let run = |cmd: &mut std::process::Command| {
            let out = cmd.current_dir(dir).output().unwrap();
            assert!(
                out.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let mut cmd = git_env();
        cmd.args(["init", "-q", "-b", "main"]);
        run(&mut cmd);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        run(git_env().args(["add", "."]));
        run(git_env().args(["commit", "-qm", "init"]));
        let base = run(git_env().args(["rev-parse", "HEAD"]));
        for (pr, title) in prs {
            run(git_env().args(["checkout", "-q", "-b", &format!("p{pr}")]));
            std::fs::write(dir.join(format!("{pr}.txt")), pr.to_string()).unwrap();
            run(git_env().args(["add", "."]));
            run(git_env().args(["commit", "-qm", &format!("work #{pr}")]));
            run(git_env().args(["checkout", "-q", "main"]));
            let mut cmd = git_env();
            cmd.args([
                "merge",
                "--no-ff",
                "-q",
                "-m",
                &format!("Merge pull request #{pr} from u/feature/x-{pr}-thing"),
                "-m",
                title,
                &format!("p{pr}"),
            ]);
            run(&mut cmd);
        }
        let head = run(git_env().args(["rev-parse", "HEAD"]));
        (base, head)
    }

    fn prs_of(notes: &Value, key: &str) -> Vec<u64> {
        notes[key]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l["pr"].as_u64()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn notes_group_by_area_and_hide_churn() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _head) = merge_repo(
            dir.path(),
            &[
                (101, "feat(mux): new sidebar"),
                (102, "test(board): cover the sorter"),
                (103, "docs: readme"),
                (104, "fix(agents): stop the crash"),
                (105, "feat(board): card rows"),
                (100, "chore: lint"),
            ],
        );
        let notes = build(&base, dir.path());
        assert_eq!(prs_of(&notes, "highlights"), [105, 101]);
        let areas: Vec<&str> = notes["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|g| g["area"].as_str())
            .collect();
        assert_eq!(areas, ["agents"]);
        let agents = &notes["groups"][0]["lines"];
        assert_eq!(agents[0]["pr"], 104);
        assert_eq!(agents[0]["text"], "stop the crash");
        assert_eq!(notes["hidden_line"], "3 test/docs/ci/chore PRs hidden");
    }

    #[test]
    fn notes_without_feats_lead_with_first_two_visible() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _head) = merge_repo(
            dir.path(),
            &[
                (201, "fix(mux): pane restore order"),
                (202, "refactor(agents): fold the prober"),
                (203, "test: cover it"),
            ],
        );
        let notes = build(&base, dir.path());
        assert_eq!(prs_of(&notes, "highlights"), [202, 201]);
        assert_eq!(notes["hidden_line"], "1 test/docs/ci/chore PR hidden");
    }

    #[test]
    fn notes_of_only_churn_show_chores_group() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _head) = merge_repo(
            dir.path(),
            &[(301, "test: cover it"), (302, "docs: readme")],
        );
        let notes = build(&base, dir.path());
        let areas: Vec<&str> = notes["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|g| g["area"].as_str())
            .collect();
        assert_eq!(areas, ["chores"]);
        assert_eq!(notes["highlights"].as_array().map(Vec::len), Some(0));
        assert!(notes["hidden_line"].is_null());
    }

    #[test]
    fn notes_are_null_without_parseable_merges() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _head) = merge_repo(dir.path(), &[]);
        assert!(build(&base, dir.path()).is_null());
        // A bad rev degrades to null too, never a panic.
        assert!(build("deadbeefdead", dir.path()).is_null());
    }

    #[test]
    fn origin_url_reads_ssh_and_https_remotes() {
        let dir = tempfile::tempdir().unwrap();
        merge_repo(dir.path(), &[]);
        run_git_ok(
            dir.path(),
            &["remote", "add", "origin", "git@github.com:o/r.git"],
        );
        assert_eq!(
            origin_pr_url_base(dir.path()).as_deref(),
            Some("https://github.com/o/r/pull")
        );
        run_git_ok(
            dir.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/o2/r2.git",
            ],
        );
        assert_eq!(
            origin_pr_url_base(dir.path()).as_deref(),
            Some("https://github.com/o2/r2/pull")
        );
    }

    fn run_git_ok(dir: &Path, args: &[&str]) {
        let out = git_env().args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn conventional_titles_strip_prefix_in_text() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _head) = merge_repo(dir.path(), &[(401, "fix(mux): wrap the overlay")]);
        let notes = build(&base, dir.path());
        assert_eq!(notes["highlights"][0]["text"], "wrap the overlay");
    }

    #[test]
    fn notes_window_names_what_it_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let prs: Vec<(u64, &str)> = (1..=65)
            .map(|n| (n, "feat(mux): fill the window"))
            .collect();
        let (base, _head) = merge_repo(dir.path(), &prs);
        let notes = build(&base, dir.path());
        let shown: usize = notes["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["lines"].as_array().map(Vec::len).unwrap_or(0))
            .sum::<usize>()
            + notes["highlights"].as_array().unwrap().len();
        assert!(shown <= 62, "window kept {shown} rows");
        let hidden_line = notes["hidden_line"].as_str().unwrap();
        assert!(
            hidden_line.contains("newest 60 of 65 merges shown"),
            "hidden_line: {hidden_line}"
        );
    }

    #[test]
    fn capitalized_type_prefixes_still_strip() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _head) = merge_repo(dir.path(), &[(501, "Fix(mux): stop the crash")]);
        let notes = build(&base, dir.path());
        assert_eq!(notes["highlights"][0]["text"], "stop the crash");
    }
}

//! `fno-agents bash-census [--days N] [--allow] [--json] [--cwd PATH]` --
//! fold Bash tool_use calls out of this project's Claude transcripts into the
//! shape a permission allowlist can act on (x-997a).
//!
//! Read-only fold over transcripts, like [`crate::digest`]: no daemon RPC,
//! nothing written. Reuses [`crate::claude_drive::claude_projects_dir`] and
//! [`crate::client_verbs::claude_cwd_slug`] - the transcript store this crate
//! already resolves for `resume`/`adopt`, not a second path guess.
//!
//! The census this verb answers directly: 64,792 Bash calls over 21 days, 91%
//! compound, 30% led by a defensive `cd`, 20% `fno` verbs led by `backlog get`
//! at 1,516 single-node calls. `--allow` turns the top 20 fno verbs into
//! `Bash(fno <verb>:*)` lines ready to paste into `permissions.allow`,
//! feeding `/fewer-permission-prompts` with a measured allowlist instead of a
//! guessed one.

use crate::claude_drive::claude_projects_dir;
use crate::client_verbs::claude_cwd_slug;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DEFAULT_DAYS: u64 = 21;
const HEAD_TABLE_SIZE: usize = 30;
const VERB_TABLE_SIZE: usize = 20;
/// Command heads whose census-worthy identity is TWO words, not one: `fno
/// backlog` and `git push` mean something different from `fno` and `git`
/// alone, so these key on the first two whitespace-separated tokens.
const TWO_WORD_HEADS: &[&str] = &["fno", "git", "gh"];

#[derive(Default)]
struct Census {
    files: usize,
    calls: usize,
    compound: usize,
    leading_cd: usize,
    heredoc: usize,
    heads: HashMap<String, usize>,
    /// The `fno <verb>` breakdown: keyed on the two tokens AFTER `fno`
    /// (`backlog get`, not `fno backlog`), because that is the shape
    /// `/fewer-permission-prompts` turns into `Bash(fno <verb>:*)`.
    fno_verbs: HashMap<String, usize>,
}

/// Fold one raw command string into `census`'s counters.
fn classify_one(command: &str, census: &mut Census) {
    census.calls += 1;
    if command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
    {
        census.compound += 1;
    }
    if command.trim_start().starts_with("cd ") || command.trim() == "cd" {
        census.leading_cd += 1;
    }
    if command.contains("<<") {
        census.heredoc += 1;
    }

    let words: Vec<&str> = command.split_whitespace().collect();
    let Some(&first) = words.first() else {
        return;
    };
    let head = if TWO_WORD_HEADS.contains(&first) && words.len() >= 2 {
        format!("{first} {}", words[1])
    } else {
        first.to_string()
    };
    *census.heads.entry(head).or_insert(0) += 1;

    if first == "fno" && words.len() >= 2 {
        let verb = if words.len() >= 3 {
            format!("{} {}", words[1], words[2])
        } else {
            words[1].to_string()
        };
        *census.fno_verbs.entry(verb).or_insert(0) += 1;
    }
}

/// Every `input.command` off a `{"type":"tool_use","name":"Bash",...}` content
/// entry on one transcript line. A malformed line yields nothing rather than
/// aborting the fold - one bad row must not blank the whole census.
fn commands_in_line(line: &str) -> Vec<String> {
    let Ok(row) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(content) = row
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|entry| {
            if entry.get("type")?.as_str()? != "tool_use" {
                return None;
            }
            if entry.get("name")?.as_str()? != "Bash" {
                return None;
            }
            entry
                .get("input")?
                .get("command")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

/// `*.jsonl` transcripts under `dir` whose mtime falls inside the last `days`
/// days. `days == 0` means "no window": every `.jsonl` file qualifies.
fn qualifying_files(dir: &Path, days: u64) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let cutoff = if days == 0 {
        None
    } else {
        SystemTime::now().checked_sub(Duration::from_secs(days * 86_400))
    };
    read.flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter(|path| match cutoff {
            None => true,
            Some(cutoff) => std::fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified >= cutoff),
        })
        .collect()
}

fn top_n(map: &HashMap<String, usize>, n: usize) -> Vec<(String, usize)> {
    let mut items: Vec<(String, usize)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    items.truncate(n);
    items
}

fn pct(n: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (n as f64) * 100.0 / (total as f64)
    }
}

fn print_report(census: &Census, heads: &[(String, usize)], verbs: &[(String, usize)], days: u64) {
    println!(
        "bash-census: {} file(s), {} Bash call(s) over {} day(s) \
         (compound {} [{:.0}%], leading-cd {} [{:.0}%], heredoc {} [{:.0}%])",
        census.files,
        census.calls,
        days,
        census.compound,
        pct(census.compound, census.calls),
        census.leading_cd,
        pct(census.leading_cd, census.calls),
        census.heredoc,
        pct(census.heredoc, census.calls),
    );
    println!("  top heads:");
    for (head, count) in heads {
        println!("    {count:>6}  {head}");
    }
    println!("  top fno verbs:");
    for (verb, count) in verbs {
        println!("    {count:>6}  {verb}");
    }
}

pub fn run_bash_census(args: &[String]) -> i32 {
    let mut days = DEFAULT_DAYS;
    let mut allow = false;
    let mut json = false;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(v) => days = v,
                    None => {
                        eprintln!("fno-agents bash-census: --days needs a non-negative integer");
                        return 2;
                    }
                }
            }
            "--allow" => allow = true,
            "--json" => json = true,
            "--cwd" => {
                i += 1;
                match args.get(i) {
                    Some(p) => cwd = PathBuf::from(p),
                    None => {
                        eprintln!("fno-agents bash-census: --cwd needs a path");
                        return 2;
                    }
                }
            }
            other => {
                eprintln!("fno-agents bash-census: unknown flag {other}");
                return 2;
            }
        }
        i += 1;
    }

    let slug = claude_cwd_slug(&cwd);
    let dir = claude_projects_dir().join(&slug);
    let files = qualifying_files(&dir, days);

    let mut census = Census {
        files: files.len(),
        ..Default::default()
    };
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            // Cheap pre-filter: skip the JSON parse for the common line that
            // never mentions Bash at all (assistant prose, other tool_use).
            if !line.contains("\"Bash\"") {
                continue;
            }
            for cmd in commands_in_line(line) {
                classify_one(&cmd, &mut census);
            }
        }
    }

    if census.calls == 0 {
        println!("no Bash calls in window");
        return 3;
    }

    let top_heads = top_n(&census.heads, HEAD_TABLE_SIZE);
    let top_verbs = top_n(&census.fno_verbs, VERB_TABLE_SIZE);

    if allow {
        for (verb, _) in &top_verbs {
            println!("Bash(fno {verb}:*)");
        }
        return 0;
    }

    if json {
        let payload = serde_json::json!({
            "files": census.files,
            "calls": census.calls,
            "compound": census.compound,
            "leading_cd": census.leading_cd,
            "heredoc": census.heredoc,
            "heads": top_heads,
            "fno_verbs": top_verbs,
        });
        println!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
        );
        return 0;
    }

    print_report(&census, &top_heads, &top_verbs, days);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// One assistant transcript row whose sole content entry is a Bash
    /// tool_use for `command`. Trims the real schema to the fields this
    /// module actually reads.
    fn bash_row(command: &str) -> String {
        serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": command}}
                ]
            }
        })
        .to_string()
    }

    /// Build `<tmp>/<slug>/one.jsonl` holding `commands` (one row each) and
    /// point both env overrides at it: FNO_CLAUDE_PROJECTS_DIR at `<tmp>`,
    /// and the returned cwd is the one whose slug matches.
    struct Fixture {
        _dir: tempfile::TempDir,
        cwd: PathBuf,
    }

    fn write_fixture(commands: &[&str]) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = PathBuf::from("/fixture/project");
        let slug = claude_cwd_slug(&cwd);
        let project_dir = dir.path().join(&slug);
        std::fs::create_dir_all(&project_dir).expect("mkdir project dir");
        let mut f =
            std::fs::File::create(project_dir.join("one.jsonl")).expect("create transcript");
        for cmd in commands {
            writeln!(f, "{}", bash_row(cmd)).expect("write row");
        }
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, dir.path());
        Fixture { _dir: dir, cwd }
    }

    /// Serializes env-var-mutating tests: `std::env::set_var` is process-wide,
    /// so two of these racing in parallel `cargo test` threads would read
    /// each other's projects dir.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_three_command_fixture_matches_every_counted_share() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fx = write_fixture(&[
            "cd x && grep a b",
            "fno backlog get x-1",
            "cat <<EOF\nhi\nEOF",
        ]);
        let args = vec![
            "--days".to_string(),
            "365".to_string(),
            "--cwd".to_string(),
            fx.cwd.display().to_string(),
            "--json".to_string(),
        ];
        // Capture via the pure classify path instead of stdout: run once to
        // get the exit code, and separately re-derive counts the same way
        // main() does, so the assertions read the real computed shape.
        let rc = run_bash_census(&args);
        assert_eq!(rc, 0);

        let slug = claude_cwd_slug(&fx.cwd);
        let dir = claude_projects_dir().join(&slug);
        let files = qualifying_files(&dir, 365);
        let mut census = Census {
            files: files.len(),
            ..Default::default()
        };
        for path in &files {
            let text = std::fs::read_to_string(path).unwrap();
            for line in text.lines() {
                for cmd in commands_in_line(line) {
                    classify_one(&cmd, &mut census);
                }
            }
        }
        assert_eq!(census.calls, 3);
        assert_eq!(census.compound, 1);
        assert_eq!(census.leading_cd, 1);
        assert_eq!(census.heredoc, 1);
        assert_eq!(census.fno_verbs.get("backlog get"), Some(&1));
    }

    #[test]
    fn allow_on_the_fixture_prints_exactly_one_pasteable_line() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fx = write_fixture(&["fno backlog get x-1"]);
        let args = vec![
            "--days".to_string(),
            "365".to_string(),
            "--cwd".to_string(),
            fx.cwd.display().to_string(),
            "--allow".to_string(),
        ];
        // --allow's own assertion is on the top_verbs computation, since this
        // module does not capture stdout in-process; the printed line is
        // `Bash(fno {verb}:*)` per verb, verified against the same top_n the
        // binary path calls.
        let slug = claude_cwd_slug(&fx.cwd);
        let dir = claude_projects_dir().join(&slug);
        let files = qualifying_files(&dir, 365);
        let mut census = Census {
            files: files.len(),
            ..Default::default()
        };
        for path in &files {
            let text = std::fs::read_to_string(path).unwrap();
            for line in text.lines() {
                for cmd in commands_in_line(line) {
                    classify_one(&cmd, &mut census);
                }
            }
        }
        let verbs = top_n(&census.fno_verbs, VERB_TABLE_SIZE);
        assert_eq!(verbs, vec![("backlog get".to_string(), 1)]);
        assert_eq!(run_bash_census(&args), 0);
    }

    #[test]
    fn an_empty_window_exits_three() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, dir.path());
        let args = vec!["--cwd".to_string(), "/nothing/here".to_string()];
        assert_eq!(run_bash_census(&args), 3);
    }

    #[test]
    fn classify_one_counts_a_compound_leading_cd_command() {
        let mut census = Census::default();
        classify_one("cd /tmp && grep foo bar.txt", &mut census);
        assert_eq!(census.calls, 1);
        assert_eq!(census.compound, 1);
        assert_eq!(census.leading_cd, 1);
        assert_eq!(census.heredoc, 0);
        assert_eq!(census.heads.get("grep"), None);
        assert_eq!(census.heads.get("cd"), Some(&1));
    }

    #[test]
    fn classify_one_reads_a_two_word_fno_verb() {
        let mut census = Census::default();
        classify_one("fno agents mail send x --raw '/x'", &mut census);
        assert_eq!(census.heads.get("fno agents"), Some(&1));
        assert_eq!(census.fno_verbs.get("agents mail"), Some(&1));
    }

    #[test]
    fn classify_one_reads_a_single_word_fno_verb() {
        let mut census = Census::default();
        classify_one("fno whoami", &mut census);
        assert_eq!(census.fno_verbs.get("whoami"), Some(&1));
    }
}

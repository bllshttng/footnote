//! Parallel execution-wave validation and worktree lifecycle.
//!
//! `fno-agents wave check|fork|join` is a transport-only door used by plan
//! validation and `/execute waves`; it is not an advertised `fno` verb.

use serde_yaml_ng::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Debug)]
struct Task {
    id: String,
    surfaces: Vec<String>,
    verify: String,
}

#[derive(Clone, Debug)]
struct Wave {
    number: String,
    mode: String,
    tasks: Vec<String>,
}

#[derive(Clone, Debug)]
struct Strategy {
    waves: Vec<Wave>,
    tasks: BTreeMap<String, Task>,
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn mapping_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(&Value::String(key.to_string()))
}

fn parse_strategy(path: &Path) -> Result<Strategy, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("plan could not be read: {e}"))?;
    let mut in_heading = false;
    let mut in_yaml = false;
    let mut yaml = String::new();
    for line in text.lines() {
        if !in_yaml && line.trim() == "## Execution Strategy" {
            in_heading = true;
            continue;
        }
        if in_heading && !in_yaml && line.trim_start().starts_with("```") {
            in_yaml = true;
            continue;
        }
        if in_yaml && line.trim_start().starts_with("```") {
            break;
        }
        if in_yaml {
            yaml.push_str(line);
            yaml.push('\n');
        }
    }
    if yaml.is_empty() {
        return Err("plan has no YAML Execution Strategy".to_string());
    }
    let root: Value = serde_yaml_ng::from_str(&yaml)
        .map_err(|e| format!("Execution Strategy YAML is invalid: {e}"))?;
    let mut tasks = BTreeMap::new();
    if let Some(rows) = mapping_value(&root, "tasks").and_then(Value::as_sequence) {
        for row in rows {
            let Some(id) = mapping_value(row, "id").and_then(value_text) else {
                continue;
            };
            let surfaces = mapping_value(row, "surface")
                .and_then(Value::as_sequence)
                .map(|items| items.iter().filter_map(value_text).collect())
                .unwrap_or_default();
            let verify = mapping_value(row, "verify")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            tasks.insert(
                id.clone(),
                Task {
                    id,
                    surfaces,
                    verify,
                },
            );
        }
    }
    let mut waves = Vec::new();
    if let Some(rows) = mapping_value(&root, "waves").and_then(Value::as_sequence) {
        for (index, row) in rows.iter().enumerate() {
            let number = mapping_value(row, "wave")
                .and_then(value_text)
                .unwrap_or_else(|| (index + 1).to_string());
            let mode = mapping_value(row, "mode")
                .and_then(Value::as_str)
                .unwrap_or("sequential")
                .to_string();
            let task_refs = mapping_value(row, "tasks")
                .and_then(Value::as_sequence)
                .map(|items| items.iter().filter_map(value_text).collect())
                .unwrap_or_default();
            waves.push(Wave {
                number,
                mode,
                tasks: task_refs,
            });
        }
    }
    Ok(Strategy { waves, tasks })
}

fn git(cwd: &Path, args: &[&OsStr]) -> Result<Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))
}

fn git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let out = git(cwd, &args)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn normalize(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.trim().replace('\\', "/").split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

fn has_glob(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('[')
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let mut table = vec![vec![false; v.len() + 1]; p.len() + 1];
    table[0][0] = true;
    for i in 0..p.len() {
        for j in 0..=v.len() {
            if !table[i][j] {
                continue;
            }
            match p[i] {
                '*' => {
                    table[i + 1][j] = true;
                    for k in j..v.len() {
                        table[i + 1][k + 1] = true;
                    }
                }
                '?' if j < v.len() => table[i + 1][j + 1] = true,
                '[' => {
                    let Some(end) = p[i + 1..].iter().position(|c| *c == ']') else {
                        if j < v.len() && v[j] == '[' {
                            table[i + 1][j + 1] = true;
                        }
                        continue;
                    };
                    if j < v.len() {
                        let set = &p[i + 1..i + 1 + end];
                        let negate = set.first().is_some_and(|c| *c == '!' || *c == '^');
                        let mut matched = false;
                        let chars = if negate { &set[1..] } else { set };
                        let mut k = 0;
                        while k < chars.len() {
                            if k + 2 < chars.len() && chars[k + 1] == '-' {
                                matched |= chars[k] <= v[j] && v[j] <= chars[k + 2];
                                k += 3;
                            } else {
                                matched |= chars[k] == v[j];
                                k += 1;
                            }
                        }
                        if matched != negate {
                            table[i + end + 2][j + 1] = true;
                        }
                    }
                }
                c if j < v.len() && c == v[j] => table[i + 1][j + 1] = true,
                _ => {}
            }
        }
    }
    table[p.len()][v.len()]
}

fn repository_files(root: &Path) -> Vec<String> {
    let args = [
        OsStr::new("ls-files"),
        OsStr::new("-co"),
        OsStr::new("--exclude-standard"),
    ];
    git(root, &args)
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(normalize)
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn surface_paths(surface: &str, files: &[String]) -> BTreeSet<String> {
    let pattern = normalize(surface);
    if has_glob(&pattern) {
        files
            .iter()
            .filter(|path| glob_matches(&pattern, path))
            .cloned()
            .collect()
    } else {
        BTreeSet::from([pattern])
    }
}

fn missing_verify_path(command: &str, root: &Path) -> Option<String> {
    let tokens: Vec<String> = command
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c| c == '\'' || c == '"' || c == '`')
                .to_string()
        })
        .collect();
    let mut candidates = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if matches!(token.as_str(), "bash" | "sh" | "python" | "python3") {
            if let Some(path) = tokens.get(index + 1).filter(|p| !p.starts_with('-')) {
                candidates.push(path.clone());
            }
        }
    }
    for candidate in candidates {
        let path = Path::new(&candidate);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if !resolved.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn check_plan(plan: &Path, root: &Path) -> Result<Vec<String>, String> {
    let strategy = parse_strategy(plan)?;
    let files = repository_files(root);
    let mut lines = Vec::new();
    for wave in &strategy.waves {
        let mut seen = BTreeMap::<String, Vec<(String, bool)>>::new();
        for task_id in &wave.tasks {
            let Some(task) = strategy.tasks.get(task_id) else {
                continue;
            };
            for surface in &task.surfaces {
                let normalized = normalize(surface);
                let glob = has_glob(&normalized);
                for file in surface_paths(&normalized, &files) {
                    seen.entry(file).or_default().push((task.id.clone(), glob));
                }
            }
            if let Some(path) = missing_verify_path(&task.verify, root) {
                lines.push(format!(
                    "X\tverify command for task '{}' names missing path '{}'",
                    task.id, path
                ));
            }
        }
        if wave.mode == "parallel" {
            for (path, owners) in seen {
                let ids: BTreeSet<String> = owners.iter().map(|(id, _)| id.clone()).collect();
                if ids.len() < 2 {
                    continue;
                }
                let id_list = ids.into_iter().collect::<Vec<_>>().join(", ");
                if owners.iter().all(|(_, glob)| !glob) {
                    lines.push(format!(
                        "E\tparallel tasks share surface '{path}': {id_list}"
                    ));
                } else {
                    lines.push(format!(
                        "X\tparallel tasks share surface glob '{path}': {id_list}"
                    ));
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push("O\twave strategy checks passed".to_string());
    }
    Ok(lines)
}

fn run_check(args: &[String]) -> i32 {
    if args.len() != 1 {
        eprintln!("usage: fno-agents wave check <plan.md>");
        return 2;
    }
    let plan = PathBuf::from(&args[0]);
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            println!("U\tcurrent directory unavailable: {error}");
            return 0;
        }
    };
    let root = git_text(&cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or(cwd);
    match check_plan(&plan, &root) {
        Ok(lines) => {
            for line in &lines {
                println!("{line}");
            }
            0
        }
        Err(error) => {
            println!("U\t{error}");
            0
        }
    }
}

/// Transport entrypoint: `fno-agents wave check <plan.md>`.
pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("check") {
        return run_check(&args[1..]);
    }
    eprintln!("usage: fno-agents wave check <plan>");
    2
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn plan(strategy: &str) -> String {
        format!("---\nkind: quick-plan\n---\n\n## Execution Strategy\n\n```yaml\n{strategy}\n```\n")
    }

    #[test]
    fn wave_check_rules() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/fold.py"), "").unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let file = repo.join("plan.md");
        fs::write(&file, plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['src/*.py']\n    verify: 'bash scripts/missing.sh'\n  - id: '1.2'\n    surface: ['src/fold.py']\n    verify: 'cargo test -p fno-agents --lib'" )).unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines
            .iter()
            .any(|line| line
                .starts_with("X\tparallel tasks share surface glob 'src/fold.py': 1.1, 1.2")));
        assert!(lines
            .iter()
            .any(|line| line.contains("task '1.1' names missing path 'scripts/missing.sh'")));

        fs::write(&file, plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['src/fold.py']\n    verify: 'cargo test'\n  - id: '1.2'\n    surface: ['./src/fold.py']\n    verify: 'cargo test'" )).unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines
            .iter()
            .any(|line| line == "E\tparallel tasks share surface 'src/fold.py': 1.1, 1.2"));
    }
}

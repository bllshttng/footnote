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

fn wave_slug(path: &Path) -> String {
    path.file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("plan")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn task_branch(repo: &Path, plan: &Path, wave: &str, task: &str) -> Result<String, String> {
    let branch = git_text(repo, &["branch", "--show-current"])?;
    let prefix = branch.trim_start_matches("feature/").replace('/', "-");
    Ok(format!(
        "wave/{prefix}/{}/{}/{}",
        wave_slug(plan),
        wave,
        task.replace('.', "-")
    ))
}

fn select_wave(strategy: &Strategy, number: &str) -> Result<&Wave, String> {
    strategy
        .waves
        .iter()
        .find(|wave| wave.number == number)
        .ok_or_else(|| format!("wave '{number}' is not declared in the plan"))
}

fn run_fork_at(cwd: &Path, plan: &Path, number: &str) -> Result<Vec<String>, String> {
    if cwd.join(".git").is_dir() {
        return Err("wave fork requires a linked target worktree; main checkout refused".into());
    }
    let status = git_text(cwd, &["status", "--porcelain", "--untracked-files=all"])?;
    if !status.is_empty() {
        return Err("wave fork requires a clean target worktree".into());
    }
    let strategy = parse_strategy(plan)?;
    let wave = select_wave(&strategy, number)?;
    if wave.mode != "parallel" || wave.tasks.len() < 2 {
        return Err("wave fork requires a parallel wave with at least two tasks".into());
    }
    let base = git_text(cwd, &["rev-parse", "HEAD"])?;
    let parent = cwd
        .parent()
        .ok_or("target worktree has no parent directory")?;
    let target_name = cwd.file_name().and_then(OsStr::to_str).unwrap_or("target");
    let mut lines = vec![format!("B\t{base}")];
    let mut created = Vec::new();
    for task_id in &wave.tasks {
        let branch = task_branch(cwd, plan, number, task_id)?;
        let worktree = parent.join(format!(
            "{target_name}-wave-{number}-{}",
            task_id.replace('.', "-")
        ));
        if worktree.exists() {
            for (path, prior_branch) in created.iter().rev() {
                let _ = git(
                    cwd,
                    &[
                        OsStr::new("worktree"),
                        OsStr::new("remove"),
                        path.as_os_str(),
                    ],
                );
                let _ = git(
                    cwd,
                    &[
                        OsStr::new("branch"),
                        OsStr::new("-D"),
                        OsStr::new(prior_branch),
                    ],
                );
            }
            return Err(format!(
                "wave task worktree already exists: {}",
                worktree.display()
            ));
        }
        let add_args = [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(&branch),
            worktree.as_os_str(),
            OsStr::new(&base),
        ];
        let out = git(cwd, &add_args)?;
        if !out.status.success() {
            for (path, prior_branch) in created.iter().rev() {
                let _ = git(
                    cwd,
                    &[
                        OsStr::new("worktree"),
                        OsStr::new("remove"),
                        path.as_os_str(),
                    ],
                );
                let _ = git(
                    cwd,
                    &[
                        OsStr::new("branch"),
                        OsStr::new("-D"),
                        OsStr::new(prior_branch),
                    ],
                );
            }
            return Err(format!(
                "could not create task {} worktree: {}",
                task_id,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        created.push((worktree.clone(), branch.clone()));
        lines.push(format!(
            "O\t{}\t{}\t{}",
            task_id,
            worktree.display(),
            branch
        ));
    }
    Ok(lines)
}

fn run_fork(plan: &Path, number: &str) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    run_fork_at(&cwd, plan, number)
}

fn append_off_surface(root: &Path, files: &[(String, String)]) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    let summary = root.join(".fno/SUMMARY.md");
    if let Some(parent) = summary.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut body = fs::read_to_string(&summary).unwrap_or_default();
    if !body.contains("## Off-surface writes") {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("\n## Off-surface writes\n");
    }
    for (task, file) in files {
        body.push_str(&format!("- task {task}: {file}\n"));
    }
    fs::write(summary, body).map_err(|e| format!("could not update SUMMARY.md: {e}"))
}

fn run_verify(root: &Path, task: &Task) -> Result<(), String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(&task.verify)
        .current_dir(root)
        .output()
        .map_err(|e| format!("task {} verify could not start: {e}", task.id))?;
    if !output.status.success() {
        return Err(format!("task {} verify failed: {}", task.id, task.verify));
    }
    Ok(())
}

fn run_join_at(cwd: &Path, plan: &Path, number: &str, base: &str) -> Result<Vec<String>, String> {
    let strategy = parse_strategy(plan)?;
    let wave = select_wave(&strategy, number)?;
    let dirty = git_text(cwd, &["status", "--porcelain", "--untracked-files=all"])?;
    if !dirty.is_empty() {
        return Err("wave join requires a clean target worktree".into());
    }
    let current = git_text(cwd, &["rev-parse", "HEAD"])?;
    if current != base {
        return Err(format!("wave base moved: expected {base}, found {current}"));
    }
    let mut off_surface = Vec::new();
    let mut branches = Vec::new();
    for task_id in &wave.tasks {
        let task = strategy
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task {task_id} is missing"))?;
        let branch = task_branch(cwd, plan, number, task_id)?;
        let branch_head = git_text(
            cwd,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .map_err(|_| format!("task {task_id} branch is missing: {branch}"))?;
        if branch_head == base {
            return Err(format!("task {task_id} has no committed work"));
        }
        let changed = git_text(cwd, &["diff", "--name-only", &format!("{base}..{branch}")])?;
        for file in changed.lines().map(normalize) {
            let allowed = task.surfaces.iter().any(|surface| {
                let surface = normalize(surface);
                if has_glob(&surface) {
                    glob_matches(&surface, &file)
                } else {
                    surface == file
                }
            });
            if !allowed {
                off_surface.push((task_id.clone(), file));
            }
        }
        branches.push((task_id.clone(), branch, branch_head));
    }
    let mut lines = Vec::new();
    for (task_id, branch, _) in &branches {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["merge", "--no-ff", "--no-edit", branch])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            let conflict =
                git_text(cwd, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
            let _ = git_text(cwd, &["merge", "--abort"]);
            let _ = git_text(cwd, &["reset", "--hard", base]);
            let file = conflict.lines().next().unwrap_or("unknown file");
            return Err(format!(
                "task {task_id} merge conflict in {file}; worktree retained"
            ));
        }
        lines.push(format!("M\t{task_id}\t{branch}"));
    }
    for (task_id, _, _) in &branches {
        let task = strategy.tasks.get(task_id).expect("task checked above");
        if let Err(error) = run_verify(cwd, task) {
            let _ = git_text(cwd, &["reset", "--hard", base]);
            return Err(error);
        }
    }
    append_off_surface(cwd, &off_surface)?;
    for (_, branch, _) in &branches {
        let out = git(
            cwd,
            &[
                OsStr::new("worktree"),
                OsStr::new("list"),
                OsStr::new("--porcelain"),
            ],
        )?;
        let text = String::from_utf8_lossy(&out.stdout);
        for block in text.split("\n\n") {
            if block
                .lines()
                .any(|line| line == format!("branch refs/heads/{branch}"))
            {
                if let Some(path) = block
                    .lines()
                    .find_map(|line| line.strip_prefix("worktree "))
                {
                    let remove = git(
                        cwd,
                        &[
                            OsStr::new("worktree"),
                            OsStr::new("remove"),
                            OsStr::new(path),
                        ],
                    )?;
                    if !remove.status.success() {
                        return Err(format!("could not remove task worktree {path}"));
                    }
                }
            }
        }
        let deleted = git(
            cwd,
            &[OsStr::new("branch"), OsStr::new("-d"), OsStr::new(branch)],
        )?;
        if !deleted.status.success() {
            return Err(format!("could not remove merged task branch {branch}"));
        }
    }
    for (task_id, file) in &off_surface {
        lines.push(format!("OFF\t{task_id}\t{file}"));
    }
    Ok(lines)
}

fn run_join(plan: &Path, number: &str, base: &str) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    run_join_at(&cwd, plan, number, base)
}

/// Transport entrypoint: `fno-agents wave check|fork|join ...`.
pub fn run(args: &[String]) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("usage: fno-agents wave check|fork|join ...");
        return 2;
    };
    let rest = &args[1..];
    let result = match command {
        "check" => return run_check(rest),
        "fork" if rest.len() == 3 && rest[1] == "--wave" => run_fork(Path::new(&rest[0]), &rest[2]),
        "join" if rest.len() == 5 && rest[1] == "--wave" && rest[3] == "--base" => {
            run_join(Path::new(&rest[0]), &rest[2], &rest[4])
        }
        _ => {
            eprintln!("usage: fno-agents wave check <plan> | fork <plan> --wave <n> | join <plan> --wave <n> --base <sha>");
            return 2;
        }
    };
    match result {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(error) => {
            println!("E\t{error}");
            1
        }
    }
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

    #[test]
    fn wave_fork_join_two_task_wave() {
        let dir = tempdir().unwrap();
        let main = dir.path().join("main");
        let target = dir.path().join("target");
        fs::create_dir_all(&main).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap()
        };
        assert!(git(&main, &["init", "-q"]).status.success());
        assert!(git(&main, &["config", "user.email", "test@example.com"])
            .status
            .success());
        assert!(git(&main, &["config", "user.name", "Test"])
            .status
            .success());
        fs::write(main.join(".gitignore"), ".fno/\n").unwrap();
        fs::write(main.join("a.txt"), "base\n").unwrap();
        assert!(git(&main, &["add", ".gitignore", "a.txt"]).status.success());
        assert!(git(&main, &["commit", "-qm", "base"]).status.success());
        assert!(git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "feature/target",
                target.to_str().unwrap()
            ]
        )
        .status
        .success());
        let plan_path = dir.path().join("wave-plan.md");
        fs::write(&plan_path, plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['a.txt']\n    verify: 'git diff --quiet'\n  - id: '1.2'\n    surface: ['b.txt']\n    verify: 'git diff --quiet'" )).unwrap();
        let fork = run_fork_at(&target, &plan_path, "1").unwrap();
        assert_eq!(
            fork.iter().filter(|line| line.starts_with("O\t")).count(),
            2
        );
        let worktrees: Vec<(String, PathBuf)> = fork
            .iter()
            .filter_map(|line| {
                let columns: Vec<&str> = line.split('\t').collect();
                (columns.len() == 4).then(|| (columns[1].to_string(), PathBuf::from(columns[2])))
            })
            .collect();
        let wt1 = &worktrees[0].1;
        let wt2 = &worktrees[1].1;
        fs::write(wt1.join("a.txt"), "task one\n").unwrap();
        assert!(git(&wt1, &["add", "a.txt"]).status.success());
        assert!(git(&wt1, &["commit", "-qm", "task one"]).status.success());
        fs::write(wt2.join("b.txt"), "task two\n").unwrap();
        assert!(git(&wt2, &["add", "b.txt"]).status.success());
        fs::write(wt2.join("c.txt"), "off surface\n").unwrap();
        assert!(git(&wt2, &["add", "c.txt"]).status.success());
        assert!(git(&wt2, &["commit", "-qm", "task two"]).status.success());
        let base = fork[0].strip_prefix("B\t").unwrap();
        let lines = run_join_at(&target, &plan_path, "1", base).unwrap();
        assert_eq!(
            lines.iter().filter(|line| line.starts_with("M\t")).count(),
            2
        );
        assert!(lines.iter().any(|line| line == "OFF\t1.2\tc.txt"));
        assert!(fs::read_to_string(target.join(".fno/SUMMARY.md"))
            .unwrap()
            .contains("- task 1.2: c.txt"));
        assert!(target.join("b.txt").is_file());
        assert!(worktrees.iter().all(|(_, path)| !path.exists()));
        for task_id in ["1.1", "1.2"] {
            let branch = task_branch(&target, &plan_path, "1", task_id).unwrap();
            assert!(
                git(
                    &target,
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{branch}")
                    ]
                )
                .status
                .code()
                    == Some(1)
            );
        }

        fs::write(
            &plan_path,
            plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\n  - wave: 2\n    mode: parallel\n    tasks: ['2.1', '2.2']\ntasks:\n  - id: '1.1'\n    surface: ['a.txt']\n    verify: 'git diff --quiet'\n  - id: '1.2'\n    surface: ['b.txt']\n    verify: 'git diff --quiet'\n  - id: '2.1'\n    surface: ['a.txt']\n    verify: 'git diff --quiet'\n  - id: '2.2'\n    surface: ['a.txt']\n    verify: 'git diff --quiet'"),
        )
        .unwrap();
        let conflict_fork = run_fork_at(&target, &plan_path, "2").unwrap();
        let conflict_worktrees: Vec<PathBuf> = conflict_fork
            .iter()
            .filter_map(|line| {
                let columns: Vec<&str> = line.split('\t').collect();
                (columns.len() == 4).then(|| PathBuf::from(columns[2]))
            })
            .collect();
        fs::write(conflict_worktrees[0].join("a.txt"), "side one\n").unwrap();
        assert!(git(&conflict_worktrees[0], &["add", "a.txt"])
            .status
            .success());
        assert!(git(&conflict_worktrees[0], &["commit", "-qm", "side one"])
            .status
            .success());
        fs::write(conflict_worktrees[1].join("a.txt"), "side two\n").unwrap();
        assert!(git(&conflict_worktrees[1], &["add", "a.txt"])
            .status
            .success());
        assert!(git(&conflict_worktrees[1], &["commit", "-qm", "side two"])
            .status
            .success());
        let conflict_base = conflict_fork[0].strip_prefix("B\t").unwrap();
        let error = run_join_at(&target, &plan_path, "2", conflict_base).unwrap_err();
        assert!(error.contains("task 2.2 merge conflict in a.txt"));
        assert_eq!(
            git_text(&target, &["rev-parse", "HEAD"]).unwrap(),
            conflict_base
        );
        assert!(conflict_worktrees.iter().all(|path| path.exists()));
    }
}

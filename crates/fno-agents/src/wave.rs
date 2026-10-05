//! Parallel execution-wave validation and worktree lifecycle.
//!
//! `fno-agents wave check|fork|join` is a transport-only door used by plan
//! validation and `/execute waves`; it is not an advertised `fno` verb.

use serde_yaml_ng::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Debug)]
struct Task {
    id: String,
    surfaces: Vec<String>,
    blocked_by: Vec<String>,
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

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Sequence(items)) => items.iter().filter_map(value_text).collect(),
        Some(item) => value_text(item).into_iter().collect(),
        None => Vec::new(),
    }
}

fn parse_strategy(path: &Path) -> Result<Strategy, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("plan could not be read: {e}"))?;
    let mut in_heading = false;
    let mut in_yaml = false;
    let mut found_yaml = false;
    let mut yaml = String::new();
    let mut section = String::new();
    for line in text.lines() {
        if !in_heading && line.trim() == "## Execution Strategy" {
            in_heading = true;
            continue;
        }
        if !in_heading {
            continue;
        }
        if !in_yaml && line.trim_start().starts_with("## ") {
            break;
        }
        if in_yaml && line.trim_start().starts_with("```") {
            break;
        }
        if !in_yaml && line.trim_start().starts_with("```yaml") {
            in_yaml = true;
            found_yaml = true;
            continue;
        }
        if in_yaml {
            yaml.push_str(line);
            yaml.push('\n');
        } else {
            section.push_str(line);
            section.push('\n');
        }
    }
    let source = if found_yaml { &yaml } else { &section };
    if source.trim().is_empty() {
        return Err("plan has no YAML Execution Strategy".to_string());
    }
    let root: Value = serde_yaml_ng::from_str(source)
        .map_err(|e| format!("Execution Strategy YAML is invalid: {e}"))?;
    let mut tasks = BTreeMap::new();
    if let Some(rows) = mapping_value(&root, "tasks").and_then(Value::as_sequence) {
        for row in rows {
            let Some(id) = mapping_value(row, "id").and_then(value_text) else {
                continue;
            };
            let surfaces = string_list(mapping_value(row, "surface"));
            let blocked_by = string_list(
                mapping_value(row, "blocked_by").or_else(|| mapping_value(row, "depends_on")),
            );
            let verify = mapping_value(row, "verify")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            tasks.insert(
                id.clone(),
                Task {
                    id,
                    surfaces,
                    blocked_by,
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
            let task_refs = string_list(mapping_value(row, "tasks"));
            waves.push(Wave {
                number,
                mode,
                tasks: task_refs,
            });
        }
    }
    Ok(Strategy { waves, tasks })
}

/// The number of waves the plan's Execution Strategy declares, or None when
/// no readable strategy declares any (no block, invalid YAML, empty waves).
/// A `#fragment` suffix is stripped first, matching the plan-doc readers.
pub fn declared_wave_count(plan: &Path) -> Option<usize> {
    let text = plan.to_str()?;
    let bare = text.split('#').next().unwrap_or(text);
    let count = parse_strategy(Path::new(bare)).ok()?.waves.len();
    (count > 0).then_some(count)
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
    let normalized = path.trim().replace('\\', "/");
    for part in normalized.split('/') {
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

#[derive(Clone)]
enum GlobToken {
    Star,
    Any,
    Literal(char),
    Class {
        negate: bool,
        ranges: Vec<(u32, u32)>,
    },
}

fn glob_tokens(pattern: &str) -> Vec<GlobToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' => tokens.push(GlobToken::Star),
            '?' => tokens.push(GlobToken::Any),
            '[' => {
                if let Some(relative_end) = chars[index + 1..].iter().position(|c| *c == ']') {
                    let end = index + 1 + relative_end;
                    let class = &chars[index + 1..end];
                    let negate = class.first().is_some_and(|c| *c == '!');
                    let mut ranges = Vec::new();
                    let mut cursor = if negate { 1 } else { 0 };
                    while cursor < class.len() {
                        if cursor + 2 < class.len() && class[cursor + 1] == '-' {
                            ranges.push((class[cursor] as u32, class[cursor + 2] as u32));
                            cursor += 3;
                        } else {
                            let value = class[cursor] as u32;
                            ranges.push((value, value));
                            cursor += 1;
                        }
                    }
                    tokens.push(GlobToken::Class { negate, ranges });
                    index = end;
                } else {
                    tokens.push(GlobToken::Literal('['));
                }
            }
            literal => tokens.push(GlobToken::Literal(literal)),
        }
        index += 1;
    }
    tokens
}

fn token_accepts(token: &GlobToken, character: char) -> bool {
    match token {
        GlobToken::Star | GlobToken::Any => true,
        GlobToken::Literal(expected) => *expected == character,
        GlobToken::Class { negate, ranges } => {
            let value = character as u32;
            ranges
                .iter()
                .any(|(start, end)| *start <= value && value <= *end)
                != *negate
        }
    }
}

fn overlapping_character(left: &GlobToken, right: &GlobToken) -> Option<char> {
    for character in ['a', 'x', '0', '/', '_', '-', '.', '!', ' '] {
        if token_accepts(left, character) && token_accepts(right, character) {
            return Some(character);
        }
    }
    let mut boundaries = vec![0x20, 0xd800, 0xe000, 0x110000];
    for token in [left, right] {
        match token {
            GlobToken::Literal(character) => {
                let value = *character as u32;
                boundaries.push(value);
                boundaries.push(value + 1);
            }
            GlobToken::Class { ranges, .. } => {
                for (start, end) in ranges {
                    boundaries.push(*start);
                    boundaries.push(end.saturating_add(1));
                }
            }
            GlobToken::Star | GlobToken::Any => {}
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    for value in boundaries {
        let Some(character) = char::from_u32(value) else {
            continue;
        };
        if !character.is_control()
            && token_accepts(left, character)
            && token_accepts(right, character)
        {
            return Some(character);
        }
    }
    None
}

fn overlapping_glob_witness(left: &str, right: &str) -> Option<String> {
    let left_pattern = left;
    let right_pattern = right;
    let left = glob_tokens(left);
    let right = glob_tokens(right);
    let mut queue = VecDeque::from([(0usize, 0usize, false, String::new())]);
    let mut visited = BTreeSet::new();
    while let Some((left_at, right_at, nonempty, witness)) = queue.pop_front() {
        if !visited.insert((left_at, right_at, nonempty)) {
            continue;
        }
        if left_at == left.len() && right_at == right.len() && nonempty {
            if crate::sync_canonical::fnmatch(&witness, left_pattern)
                && crate::sync_canonical::fnmatch(&witness, right_pattern)
            {
                return Some(witness);
            }
        }
        if matches!(left.get(left_at), Some(GlobToken::Star)) {
            queue.push_back((left_at + 1, right_at, nonempty, witness.clone()));
        }
        if matches!(right.get(right_at), Some(GlobToken::Star)) {
            queue.push_back((left_at, right_at + 1, nonempty, witness.clone()));
        }
        let (Some(left_token), Some(right_token)) = (left.get(left_at), right.get(right_at)) else {
            continue;
        };
        if let Some(character) = overlapping_character(left_token, right_token) {
            let mut next_witness = witness;
            next_witness.push(character);
            let next_left = if matches!(left_token, GlobToken::Star) {
                left_at
            } else {
                left_at + 1
            };
            let next_right = if matches!(right_token, GlobToken::Star) {
                right_at
            } else {
                right_at + 1
            };
            queue.push_back((next_left, next_right, true, next_witness));
        }
    }
    None
}

fn repository_files(root: &Path) -> Result<Vec<String>, String> {
    let args = [
        OsStr::new("ls-files"),
        OsStr::new("-co"),
        OsStr::new("--exclude-standard"),
    ];
    let out = git(root, &args)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(normalize)
        .filter(|line| !line.is_empty())
        .collect())
}

fn surface_paths(surface: &str, files: &[String]) -> BTreeSet<String> {
    let pattern = normalize(surface);
    if has_glob(&pattern) {
        files
            .iter()
            .filter(|path| crate::sync_canonical::fnmatch(path, &pattern))
            .cloned()
            .collect()
    } else {
        BTreeSet::from([pattern])
    }
}

fn path_candidate(token: &str) -> bool {
    token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.contains('/')
        || token
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
}

fn missing_verify_path(command: &str, surfaces: &[String], root: &Path) -> Option<String> {
    for segment in command.split(|c| matches!(c, '&' | '|' | ';')) {
        let tokens: Vec<String> = segment
            .split_whitespace()
            .map(|token| {
                token
                    .trim_matches(|c: char| "'\"`,;|&()".contains(c))
                    .to_string()
            })
            .collect();
        let mut skip_output_target = false;
        for (index, raw) in tokens.iter().enumerate() {
            if skip_output_target {
                skip_output_target = false;
                continue;
            }
            if matches!(raw.as_str(), ">" | ">>" | "1>" | "1>>" | "2>" | "2>>") {
                skip_output_target = true;
                continue;
            }
            if raw.starts_with('>') || raw.starts_with("1>") || raw.starts_with("2>") {
                continue;
            }
            let mut token = raw.clone();
            if let Some(path) = token.strip_prefix('<') {
                token = path.to_string();
            } else if let Some(path) = token.strip_prefix("0<") {
                token = path.to_string();
            }
            if let Some((flag, value)) = token.split_once('=') {
                if flag.starts_with('-') {
                    token = value.to_string();
                } else {
                    continue;
                }
            }
            if let Some((path, _)) = token.split_once("::") {
                token = path.to_string();
            }
            if token.is_empty()
                || token.starts_with('-')
                || token.starts_with('$')
                || token.starts_with("$(")
                || token == "origin/main"
                || token.starts_with("origin/main..")
                || token.contains("://")
                || !path_candidate(&token)
            {
                continue;
            }
            if surfaces.iter().any(|surface| {
                let surface = normalize(surface);
                let token_path = normalize(&token);
                if has_glob(&surface) {
                    crate::sync_canonical::fnmatch(&token_path, &surface)
                } else {
                    surface == token_path
                }
            }) {
                continue;
            }
            let interpreted_script = tokens[..index]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, prior)| matches!(prior.as_str(), "bash" | "sh" | "python" | "python3"))
                .is_some_and(|(at, _)| {
                    tokens[at + 1..index]
                        .iter()
                        .all(|argument| argument.starts_with('-'))
                });
            let has_extension = token
                .rsplit('/')
                .next()
                .and_then(|name| name.rsplit_once('.'))
                .is_some_and(|(stem, extension)| !stem.is_empty() && !extension.is_empty());
            let resolved = if let Some(rest) = token.strip_prefix("~/") {
                std::env::var_os("HOME").map(PathBuf::from)?.join(rest)
            } else {
                let path = Path::new(&token);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    root.join(path)
                }
            };
            if !resolved.exists() || ((interpreted_script || has_extension) && !resolved.is_file())
            {
                return Some(token);
            }
        }
    }
    None
}

fn check_plan(plan: &Path, root: &Path) -> Result<Vec<String>, String> {
    let strategy = parse_strategy(plan)?;
    let files = repository_files(root)?;
    let mut lines = Vec::new();
    let task_waves: BTreeMap<String, usize> = strategy
        .waves
        .iter()
        .enumerate()
        .flat_map(|(index, wave)| wave.tasks.iter().map(move |task| (task.clone(), index)))
        .collect();
    let task_surfaces: Vec<String> = strategy
        .tasks
        .values()
        .flat_map(|task| task.surfaces.iter().cloned())
        .collect();
    for (wave_index, wave) in strategy.waves.iter().enumerate() {
        let mut seen = BTreeMap::<String, Vec<(String, bool)>>::new();
        let mut glob_details = BTreeMap::<String, (String, String)>::new();
        let mut surfaces = Vec::<(String, String, bool)>::new();
        for task_id in &wave.tasks {
            let Some(task) = strategy.tasks.get(task_id) else {
                continue;
            };
            for dependency in &task.blocked_by {
                if task_waves
                    .get(dependency)
                    .is_some_and(|dependency_wave| *dependency_wave > wave_index)
                {
                    lines.push(format!(
                        "X\ttask {} blocked_by {}, which runs in a later wave",
                        task.id, dependency
                    ));
                }
            }
            for surface in &task.surfaces {
                let normalized = normalize(surface);
                let glob = has_glob(&normalized);
                surfaces.push((task.id.clone(), normalized.clone(), glob));
                for file in surface_paths(&normalized, &files) {
                    seen.entry(file).or_default().push((task.id.clone(), glob));
                }
            }
            if let Some(path) = missing_verify_path(&task.verify, &task_surfaces, root) {
                lines.push(format!(
                    "X\ttask {} verify names {}, which is not on disk and no task surface creates it",
                    task.id, path
                ));
            }
        }
        if wave.mode == "parallel" {
            for left in 0..surfaces.len() {
                for right in left + 1..surfaces.len() {
                    let (left_id, left_path, left_glob) = &surfaces[left];
                    let (right_id, right_path, right_glob) = &surfaces[right];
                    if left_id == right_id {
                        continue;
                    }
                    if *left_glob && *right_glob {
                        if let Some(witness) = overlapping_glob_witness(left_path, right_path) {
                            let owners = seen.entry(witness.clone()).or_default();
                            owners.push((left_id.clone(), true));
                            owners.push((right_id.clone(), true));
                            glob_details.insert(witness, (left_path.clone(), right_path.clone()));
                        }
                        continue;
                    }
                    if left_glob == right_glob {
                        continue;
                    }
                    let (pattern, exact, pattern_id, exact_id) = if *left_glob {
                        (left_path, right_path, left_id, right_id)
                    } else {
                        (right_path, left_path, right_id, left_id)
                    };
                    if crate::sync_canonical::fnmatch(exact, pattern) {
                        let owners = seen.entry(exact.clone()).or_default();
                        owners.push((pattern_id.clone(), true));
                        owners.push((exact_id.clone(), false));
                        glob_details.insert(exact.clone(), (pattern.clone(), exact.clone()));
                    }
                }
            }
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
                } else if let Some((left, right)) = glob_details.get(&path) {
                    lines.push(format!(
                        "X\tparallel tasks share surface '{left}' ~ '{right}': {id_list}"
                    ));
                } else {
                    lines.push(format!(
                        "X\tparallel tasks share surface '{path}': {id_list}"
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
    let mut plan = None;
    let mut repo = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--repo" {
            index += 1;
            let Some(path) = args.get(index) else {
                eprintln!("usage: fno-agents wave check <plan.md> [--repo <dir>]");
                return 2;
            };
            repo = Some(PathBuf::from(path));
        } else if let Some(path) = argument.strip_prefix("--repo=") {
            repo = Some(PathBuf::from(path));
        } else if argument.starts_with('-') || plan.is_some() {
            eprintln!("usage: fno-agents wave check <plan.md> [--repo <dir>]");
            return 2;
        } else {
            plan = Some(PathBuf::from(argument));
        }
        index += 1;
    }
    let Some(plan) = plan else {
        eprintln!("usage: fno-agents wave check <plan.md> [--repo <dir>]");
        return 2;
    };
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            println!("U\tcurrent directory unavailable: {error}");
            return 0;
        }
    };
    let root_path = repo.as_deref().unwrap_or(&cwd);
    let root = match git_text(root_path, &["rev-parse", "--show-toplevel"]) {
        Ok(path) => PathBuf::from(path),
        Err(_) if repo.is_some() => {
            println!("W\trepository root could not be resolved; verify and glob checks skipped");
            return 0;
        }
        Err(_) => {
            println!("W\trepository root could not be resolved; verify and glob checks skipped");
            return 0;
        }
    };
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

fn task_branch(repo: &Path, task: &str) -> Result<String, String> {
    let branch = git_text(repo, &["branch", "--show-current"])?;
    Ok(format!("{branch}-t{}", task.replace('.', "-")))
}

fn task_worktree_path(repo: &Path, task: &str) -> Result<PathBuf, String> {
    let root = PathBuf::from(git_text(repo, &["rev-parse", "--show-toplevel"])?);
    let parent = root
        .parent()
        .ok_or("target checkout has no parent directory")?;
    let name = root
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or("target checkout has no directory name")?;
    Ok(parent.join(format!("{name}-t{}", task.replace('.', "-"))))
}

fn select_wave<'a>(strategy: &'a Strategy, number: &str) -> Result<&'a Wave, String> {
    strategy
        .waves
        .iter()
        .find(|wave| wave.number == number)
        .ok_or_else(|| format!("wave '{number}' is not declared in the plan"))
}

fn selected_task_ids(wave: &Wave, requested: &[String]) -> Result<Vec<String>, String> {
    let selected: &[String] = if requested.is_empty() {
        &wave.tasks
    } else {
        requested
    };
    if selected.is_empty() {
        return Err(format!("wave '{}' has no tasks", wave.number));
    }
    let mut seen = BTreeSet::new();
    for task in selected {
        if !wave.tasks.contains(task) {
            return Err(format!("task {task} is not in wave '{}'", wave.number));
        }
        if !seen.insert(task) {
            return Err(format!("task {task} was selected more than once"));
        }
    }
    Ok(selected.to_vec())
}

fn cleanup_created_wave_worktrees(root: &Path, created: &[(PathBuf, String, bool)]) {
    for (path, branch, new_branch) in created.iter().rev() {
        let _ = git(
            root,
            &[
                OsStr::new("worktree"),
                OsStr::new("remove"),
                path.as_os_str(),
            ],
        );
        if *new_branch {
            let _ = git(
                root,
                &[OsStr::new("branch"), OsStr::new("-D"), OsStr::new(branch)],
            );
        }
    }
}

fn branch_descends_from(root: &Path, base: &str, branch: &str) -> bool {
    git(
        root,
        &[
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(base),
            OsStr::new(branch),
        ],
    )
    .is_ok_and(|output| output.status.success())
}

fn run_fork_at(
    cwd: &Path,
    plan: &Path,
    number: &str,
    requested_tasks: &[String],
) -> Result<Vec<String>, String> {
    let cwd = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"])?);
    let git_dir = git_text(&cwd, &["rev-parse", "--git-dir"])?;
    let common_dir = git_text(&cwd, &["rev-parse", "--git-common-dir"])?;
    if git_dir == common_dir {
        return Err("wave fork requires a linked target worktree; main checkout refused".into());
    }
    let status = git_text(&cwd, &["status", "--porcelain", "--untracked-files=no"])?;
    if !status.is_empty() {
        return Err("wave fork requires a clean target worktree".into());
    }
    let strategy = parse_strategy(plan)?;
    let wave = select_wave(&strategy, number)?;
    let task_ids = selected_task_ids(wave, requested_tasks)?;
    if wave.mode != "parallel" || wave.tasks.len() < 2 {
        return Err("wave fork requires a parallel wave with at least two tasks".into());
    }
    let base = git_text(&cwd, &["rev-parse", "HEAD"])?;
    let mut lines = vec![format!("B\t{base}")];
    let mut planned = Vec::new();
    for task_id in &task_ids {
        let branch = task_branch(&cwd, task_id)?;
        let worktree = task_worktree_path(&cwd, task_id)?;
        if worktree.exists() {
            let registered = worktree_for_branch(&cwd, &branch)?;
            let current_branch = git_text(&worktree, &["branch", "--show-current"])?;
            if registered.as_deref() != Some(worktree.as_path()) || current_branch != branch {
                return Err(format!(
                    "wave task worktree is occupied: {}",
                    worktree.display()
                ));
            }
            if !branch_descends_from(&cwd, &base, &branch) {
                return Err(format!(
                    "task {task_id} branch does not descend from wave base"
                ));
            }
            if !git_text(
                &worktree,
                &["status", "--porcelain", "--untracked-files=all"],
            )?
            .is_empty()
            {
                return Err(format!("task {task_id} worktree has uncommitted changes"));
            }
            planned.push((task_id.clone(), branch, worktree, true, false));
            continue;
        }
        if let Some(registered) = worktree_for_branch(&cwd, &branch)? {
            let current_branch = git_text(&registered, &["branch", "--show-current"])?;
            if current_branch != branch || !branch_descends_from(&cwd, &base, &branch) {
                return Err(format!(
                    "task {task_id} existing worktree is not reusable: {}",
                    registered.display()
                ));
            }
            if !git_text(
                &registered,
                &["status", "--porcelain", "--untracked-files=all"],
            )?
            .is_empty()
            {
                return Err(format!("task {task_id} worktree has uncommitted changes"));
            }
            planned.push((task_id.clone(), branch, registered, true, false));
            continue;
        }
        let ref_name = format!("refs/heads/{branch}");
        let branch_exists = git(
            &cwd,
            &[
                OsStr::new("show-ref"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new(&ref_name),
            ],
        )?
        .status
        .success();
        if branch_exists && !branch_descends_from(&cwd, &base, &branch) {
            return Err(format!(
                "task {task_id} branch does not descend from wave base"
            ));
        }
        planned.push((task_id.clone(), branch, worktree, false, branch_exists));
    }
    let mut created = Vec::<(PathBuf, String, bool)>::new();
    for (task_id, branch, worktree, reused, branch_exists) in planned {
        if !reused {
            let add_args = if branch_exists {
                vec![
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    worktree.as_os_str(),
                    OsStr::new(&branch),
                ]
            } else {
                vec![
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    OsStr::new("-b"),
                    OsStr::new(&branch),
                    worktree.as_os_str(),
                    OsStr::new(&base),
                ]
            };
            let out = git(&cwd, &add_args)?;
            if !out.status.success() {
                cleanup_created_wave_worktrees(&cwd, &created);
                return Err(format!(
                    "could not create task {task_id} worktree: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            created.push((worktree.clone(), branch.clone(), !branch_exists));
        }
        lines.push(format!(
            "O\t{}\t{}\t{}",
            task_id,
            worktree.display(),
            branch
        ));
    }
    Ok(lines)
}

fn run_fork(plan: &Path, number: &str, requested_tasks: &[String]) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    run_fork_at(&cwd, plan, number, requested_tasks)
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

fn worktree_for_branch(root: &Path, branch: &str) -> Result<Option<PathBuf>, String> {
    let out = git(
        root,
        &[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
        ],
    )?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for block in text.split("\n\n") {
        if block
            .lines()
            .any(|line| line == format!("branch refs/heads/{branch}"))
        {
            return Ok(block
                .lines()
                .find_map(|line| line.strip_prefix("worktree "))
                .map(PathBuf::from));
        }
    }
    Ok(None)
}

fn run_join_at(
    cwd: &Path,
    plan: &Path,
    number: &str,
    base: &str,
    requested_tasks: &[String],
) -> Result<Vec<String>, String> {
    let cwd = PathBuf::from(git_text(cwd, &["rev-parse", "--show-toplevel"])?);
    let strategy = parse_strategy(plan)?;
    let wave = select_wave(&strategy, number)?;
    let task_ids = selected_task_ids(wave, requested_tasks)?;
    if wave.mode != "parallel" || wave.tasks.len() < 2 {
        return Err("wave join requires a parallel wave with at least two tasks".into());
    }
    let dirty = git_text(&cwd, &["status", "--porcelain", "--untracked-files=all"])?;
    if !dirty.is_empty() {
        return Err("wave join requires a clean target worktree".into());
    }
    let current = git_text(&cwd, &["rev-parse", "HEAD"])?;
    if current != base {
        return Err(format!("wave base moved: expected {base}, found {current}"));
    }
    let mut off_surface = Vec::new();
    let mut branches = Vec::new();
    for task_id in &task_ids {
        let task = strategy
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task {task_id} is missing"))?;
        let branch = task_branch(&cwd, task_id)?;
        let branch_head = git_text(
            &cwd,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .map_err(|_| format!("task {task_id} branch is missing: {branch}"))?;
        let worktree = worktree_for_branch(&cwd, &branch)?
            .ok_or_else(|| format!("task {task_id} worktree is missing: {branch}"))?;
        let worktree_status = git_text(
            &worktree,
            &["status", "--porcelain", "--untracked-files=all"],
        )?;
        if !worktree_status.is_empty() {
            return Err(format!(
                "task {task_id} worktree is dirty: {}",
                worktree.display()
            ));
        }
        if branch_head == base {
            return Err(format!("task {task_id} has no committed work"));
        }
        let changed = git_text(
            &cwd,
            &["diff", "--name-only", &format!("{base}...{branch}")],
        )?;
        for file in changed.lines().map(normalize) {
            let allowed = task.surfaces.iter().any(|surface| {
                let surface = normalize(surface);
                if has_glob(&surface) {
                    crate::sync_canonical::fnmatch(&file, &surface)
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
            .arg(&cwd)
            .args(["merge", "--no-ff", "--no-edit", branch.as_str()])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            let conflict = git_text(&cwd, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .lines()
                .collect::<Vec<_>>()
                .join(", ");
            let _ = git_text(&cwd, &["merge", "--abort"]);
            let _ = git_text(&cwd, &["reset", "--hard", base]);
            return Err(format!(
                "merge conflict: task {task_id} ({})",
                if conflict.is_empty() {
                    "unknown file"
                } else {
                    &conflict
                }
            ));
        }
        let merge_sha = git_text(&cwd, &["rev-parse", "HEAD"])?;
        lines.push(format!("M\t{task_id}\t{merge_sha}"));
    }
    for (task_id, _, _) in &branches {
        let task = strategy.tasks.get(task_id).expect("task checked above");
        if let Err(error) = run_verify(&cwd, task) {
            let _ = git_text(&cwd, &["reset", "--hard", base]);
            return Err(error);
        }
    }
    if let Err(error) = append_off_surface(&cwd, &off_surface) {
        let _ = git_text(&cwd, &["reset", "--hard", base]);
        return Err(error);
    }
    for (_, branch, _) in &branches {
        if let Some(path) = worktree_for_branch(&cwd, branch)? {
            let remove = git(
                &cwd,
                &[
                    OsStr::new("worktree"),
                    OsStr::new("remove"),
                    path.as_os_str(),
                ],
            )?;
            if !remove.status.success() {
                return Err(format!("could not remove task worktree {}", path.display()));
            }
        }
        let deleted = git(
            &cwd,
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

fn run_join(
    plan: &Path,
    number: &str,
    base: &str,
    requested_tasks: &[String],
) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    run_join_at(&cwd, plan, number, base, requested_tasks)
}

fn parse_wave_operation_args(
    args: &[String],
    needs_base: bool,
) -> Result<(PathBuf, String, String, Vec<String>), String> {
    let mut plan = None;
    let mut wave = None;
    let mut base = None;
    let mut tasks = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let take = |name: &str| -> Option<String> {
            argument
                .strip_prefix(&format!("{name}="))
                .map(str::to_string)
        };
        if argument == "--wave" {
            index += 1;
            wave = args.get(index).cloned();
        } else if let Some(value) = take("--wave") {
            wave = Some(value);
        } else if argument == "--base" {
            index += 1;
            base = args.get(index).cloned();
        } else if let Some(value) = take("--base") {
            base = Some(value);
        } else if argument == "--task" {
            index += 1;
            let Some(task) = args.get(index) else {
                return Err("--task requires a task id".to_string());
            };
            tasks.push(task.clone());
        } else if let Some(task) = take("--task") {
            tasks.push(task);
        } else if argument.starts_with('-') || plan.is_some() {
            return Err(format!("unexpected wave argument: {argument}"));
        } else {
            plan = Some(PathBuf::from(argument));
        }
        index += 1;
    }
    let plan = plan.ok_or_else(|| "plan path is required".to_string())?;
    let wave = wave.ok_or_else(|| "--wave is required".to_string())?;
    let base = if needs_base {
        base.ok_or_else(|| "--base is required".to_string())?
    } else {
        base.unwrap_or_default()
    };
    Ok((plan, wave, base, tasks))
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
        "fork" => match parse_wave_operation_args(rest, false) {
            Ok((plan, wave, _, tasks)) => run_fork(&plan, &wave, &tasks),
            Err(error) => {
                eprintln!("wave fork: {error}");
                return 2;
            }
        },
        "join" => match parse_wave_operation_args(rest, true) {
            Ok((plan, wave, base, tasks)) => run_join(&plan, &wave, &base, &tasks),
            Err(error) => {
                eprintln!("wave join: {error}");
                return 2;
            }
        },
        _ => {
            eprintln!("usage: fno-agents wave check <plan> [--repo <dir>] | fork <plan> --wave <n> [--task <id> ...] | join <plan> --wave <n> --base <sha> [--task <id> ...]");
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
            if command == "fork" {
                2
            } else {
                1
            }
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
        assert!(lines.iter().any(|line| line
            .starts_with("X\tparallel tasks share surface 'src/*.py' ~ 'src/fold.py': 1.1, 1.2")));
        assert!(lines
            .iter()
            .any(|line| line == "X\ttask 1.1 verify names scripts/missing.sh, which is not on disk and no task surface creates it"));

        fs::write(
            &file,
            plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['src/*.py']\n    verify: 'cargo test'\n  - id: '1.2'\n    surface: ['src/not-created.py']\n    verify: 'cargo test'"),
        )
        .unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines.iter().any(|line| {
            line.starts_with(
                "X\tparallel tasks share surface 'src/*.py' ~ 'src/not-created.py': 1.1, 1.2",
            )
        }));

        fs::write(
            &file,
            plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['src/*.py']\n    verify: 'cargo test'\n  - id: '1.2'\n    surface: ['src/not*.py']\n    verify: 'cargo test'"),
        )
        .unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines.iter().any(|line| {
            line.starts_with("X\tparallel tasks share surface 'src/*.py' ~ 'src/not*.py': 1.1, 1.2")
        }));

        fs::write(
            &file,
            plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['a[!a-z].txt']\n    verify: 'cargo test'\n  - id: '1.2'\n    surface: ['a[!0-9].txt']\n    verify: 'cargo test'"),
        )
        .unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines.iter().any(|line| {
            line.starts_with(
                "X\tparallel tasks share surface 'a[!a-z].txt' ~ 'a[!0-9].txt': 1.1, 1.2",
            )
        }));

        fs::write(
            &file,
            plan("execution_mode: sequential\nwaves:\n  - wave: 1\n    mode: sequential\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['other.py']\n    verify: 'bash -e scripts/generated.sh'\n  - id: '1.2'\n    surface: ['scripts/generated.sh']\n    verify: 'curl https://example.com && uv run pytest cli/tests/not-there.py'"),
        )
        .unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines.iter().any(|line| {
            line == "X\ttask 1.2 verify names cli/tests/not-there.py, which is not on disk and no task surface creates it"
        }));
        assert!(lines
            .iter()
            .all(|line| !line.contains("scripts/generated.sh")));

        fs::write(
            &file,
            plan("execution_mode: sequential\nwaves:\n  - wave: 1\n    mode: sequential\n    tasks: ['1.1']\n  - wave: 2\n    mode: sequential\n    tasks: ['2.1']\ntasks:\n  - id: '1.1'\n    surface: ['a.py']\n    blocked_by: ['2.1']\n    verify: 'cargo test'\n  - id: '2.1'\n    surface: ['b.py']\n    verify: 'cargo test'"),
        )
        .unwrap();
        let lines = check_plan(&file, &repo).unwrap();
        assert!(lines
            .iter()
            .any(|line| { line == "X\ttask 1.1 blocked_by 2.1, which runs in a later wave" }));

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
        fs::write(&plan_path, plan("execution_mode: parallel\nwaves:\n  - wave: 1\n    mode: parallel\n    tasks: ['1.1', '1.2']\ntasks:\n  - id: '1.1'\n    surface: ['a.txt']\n    verify: 'printf x >> .fno/verify-one.log'\n  - id: '1.2'\n    surface: ['b.txt']\n    verify: 'printf x >> .fno/verify-two.log'" )).unwrap();
        let selected = run_fork_at(&target, &plan_path, "1", &["1.1".to_string()]).unwrap();
        assert_eq!(
            selected
                .iter()
                .filter(|line| line.starts_with("O\t"))
                .count(),
            1
        );
        let selected_branch = task_branch(&target, "1.1").unwrap();
        let selected_path = task_worktree_path(&target, "1.1").unwrap();
        assert!(git(
            &target,
            &["worktree", "remove", selected_path.to_str().unwrap()]
        )
        .status
        .success());
        assert!(git(&target, &["branch", "-d", &selected_branch])
            .status
            .success());

        let fork = run_fork_at(&target, &plan_path, "1", &[]).unwrap();
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
        let task_one_commit = git_text(wt1, &["rev-parse", "HEAD"]).unwrap();
        let task_two_commit = git_text(wt2, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(run_fork_at(&target, &plan_path, "1", &[]).unwrap(), fork);
        fs::create_dir_all(target.join(".fno")).unwrap();
        let base = fork[0].strip_prefix("B\t").unwrap();
        let lines = run_join_at(&target, &plan_path, "1", base, &[]).unwrap();
        assert_eq!(
            lines.iter().filter(|line| line.starts_with("M\t")).count(),
            2
        );
        assert!(lines.iter().any(|line| line == "OFF\t1.2\tc.txt"));
        assert!(fs::read_to_string(target.join(".fno/SUMMARY.md"))
            .unwrap()
            .contains("- task 1.2: c.txt"));
        assert!(target.join("b.txt").is_file());
        assert_eq!(
            fs::read_to_string(target.join(".fno/verify-one.log")).unwrap(),
            "x"
        );
        assert_eq!(
            fs::read_to_string(target.join(".fno/verify-two.log")).unwrap(),
            "x"
        );
        for commit in [&task_one_commit, &task_two_commit] {
            assert!(git(
                &target,
                &["merge-base", "--is-ancestor", commit.as_str(), "HEAD",]
            )
            .status
            .success());
        }
        assert!(worktrees.iter().all(|(_, path)| !path.exists()));
        for task_id in ["1.1", "1.2"] {
            let branch = task_branch(&target, task_id).unwrap();
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
        let conflict_fork = run_fork_at(&target, &plan_path, "2", &[]).unwrap();
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
        fs::write(conflict_worktrees[1].join("dirty.txt"), "uncommitted\n").unwrap();
        let dirty_error = run_join_at(&target, &plan_path, "2", conflict_base, &[]).unwrap_err();
        assert!(dirty_error.contains("task 2.2 worktree is dirty:"));
        fs::remove_file(conflict_worktrees[1].join("dirty.txt")).unwrap();
        let error = run_join_at(&target, &plan_path, "2", conflict_base, &[]).unwrap_err();
        assert!(error.contains("merge conflict: task 2.2 (a.txt)"));
        assert_eq!(
            git_text(&target, &["rev-parse", "HEAD"]).unwrap(),
            conflict_base
        );
        assert!(conflict_worktrees.iter().all(|path| path.exists()));
    }
}

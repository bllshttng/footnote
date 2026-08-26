use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ManifestIdentity {
    pub(crate) harness: String,
    pub(crate) harness_session_id: String,
    pub(crate) claude_session_id: String,
    pub(crate) codex_thread_id: String,
    pub(crate) owner_cwd: String,
    pub(crate) fno_id: String,
    pub(crate) manifest_path: PathBuf,
}

impl ManifestIdentity {
    fn session_ids(&self) -> [&str; 3] {
        [
            self.harness_session_id.as_str(),
            self.claude_session_id.as_str(),
            self.codex_thread_id.as_str(),
        ]
    }

    pub(crate) fn matches(&self, session_id: &str) -> bool {
        let sid = session_id.trim();
        !sid.is_empty()
            && self
                .session_ids()
                .iter()
                .any(|field| !field.is_empty() && field.trim() == sid)
    }

    pub(crate) fn canonical_session_id(&self) -> &str {
        self.session_ids()
            .into_iter()
            .find(|field| !field.is_empty())
            .unwrap_or("")
    }
}

#[derive(Debug)]
pub(crate) enum ManifestLookupError {
    CurrentDirectory,
    WorktreeList,
}

fn set_first(slot: &mut String, val: &str) {
    if slot.is_empty() && !val.is_empty() && val != "null" {
        *slot = val.to_string();
    }
}

pub(crate) fn parse_manifest_identity(content: &str) -> ManifestIdentity {
    let mut manifest = ManifestIdentity::default();
    let mut in_input_scalar = false;
    for line in content.lines() {
        let line = line.trim();
        let line_untrusted = in_input_scalar;
        if in_input_scalar && line_closes_quoted_scalar(line) {
            in_input_scalar = false;
        }
        if line.is_empty() || line.starts_with('#') || line == "---" {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let raw = value.trim();
        if !line_untrusted
            && key == "input"
            && raw.starts_with('"')
            && !(raw.len() >= 2 && line_closes_quoted_scalar(raw))
        {
            in_input_scalar = true;
        }
        if line_untrusted {
            continue;
        }
        let value = raw.trim_matches(|c| c == '"' || c == '\'');
        match key {
            "harness" => set_first(&mut manifest.harness, value),
            "harness_session_id" => set_first(&mut manifest.harness_session_id, value),
            "claude_session_id" => set_first(&mut manifest.claude_session_id, value),
            "codex_thread_id" => set_first(&mut manifest.codex_thread_id, value),
            "owner_cwd" => set_first(&mut manifest.owner_cwd, value),
            "fno_id" => set_first(&mut manifest.fno_id, value),
            _ => {}
        }
    }
    manifest
}

fn line_closes_quoted_scalar(raw: &str) -> bool {
    let Some(rest) = raw.strip_suffix('"') else {
        return false;
    };
    !rest.ends_with('\\')
}

pub(crate) fn git_worktree_paths(cwd: &Path) -> Result<Vec<PathBuf>, ManifestLookupError> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|_| ManifestLookupError::WorktreeList)?;
    if !output.status.success() {
        return Err(ManifestLookupError::WorktreeList);
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| ManifestLookupError::WorktreeList)?;
    let mut paths = Vec::new();
    for record in stdout.split("\n\n") {
        let mut lines = record.lines();
        let Some(first) = lines.next() else {
            continue;
        };
        let Some(path_str) = first.strip_prefix("worktree ") else {
            continue;
        };
        let path_str = path_str.trim();
        if lines.any(|line| line.trim() == "bare") || path_str.is_empty() {
            continue;
        }
        paths.push(PathBuf::from(path_str));
    }
    Ok(paths)
}

pub(crate) fn paths_eq(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => a == b,
    }
}

pub(crate) fn find_manifest_for_session(
    session_id: &str,
) -> Result<Option<ManifestIdentity>, ManifestLookupError> {
    let cwd = std::env::current_dir().map_err(|_| ManifestLookupError::CurrentDirectory)?;
    let mut candidates = git_worktree_paths(&cwd)?;
    if !candidates.iter().any(|path| paths_eq(path, &cwd)) {
        candidates.push(cwd);
    }
    for worktree in candidates {
        let manifest_path = worktree.join(".fno/target-state.md");
        let Ok(content) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let mut identity = parse_manifest_identity(&content);
        if identity.matches(session_id) {
            if identity.owner_cwd.is_empty() {
                identity.owner_cwd = worktree.to_string_lossy().into_owned();
            }
            identity.manifest_path = fs::canonicalize(&manifest_path).unwrap_or(manifest_path);
            return Ok(Some(identity));
        }
    }
    Ok(None)
}

pub fn run_manifest_for_session(args: &[String]) -> i32 {
    let mut session_id: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--harness-session-id" {
            index += 1;
            session_id = args.get(index).cloned();
        } else if let Some(value) = arg.strip_prefix("--harness-session-id=") {
            session_id = Some(value.to_string());
        } else {
            eprintln!("manifest-for-session: unknown argument: {arg}");
            return 2;
        }
        index += 1;
    }
    let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) else {
        eprintln!("manifest-for-session: --harness-session-id is required");
        return 2;
    };
    match find_manifest_for_session(&session_id) {
        Ok(Some(identity)) => {
            println!("{}", identity.manifest_path.display());
            0
        }
        Ok(None) => 1,
        Err(_) => {
            eprintln!("manifest-for-session: unable to read git worktree list");
            2
        }
    }
}

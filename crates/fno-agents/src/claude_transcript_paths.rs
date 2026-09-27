//! The claude transcript resolver behind the provider-cap sweep.
//!
//! One native answer for every non-codex row: each id (a full uuid or the
//! 8-hex short id the registry row carries) resolves against the claude
//! projects store the way `fno agents peek` resolves. This used to be a
//! hidden Python command (`fno agents transcript-paths`) the sweep shelled
//! out to; the resolver lives here now and the Python command is gone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::provider_cap::ClaudePaths;

/// One store hit: `<projects>/*/<id>*.jsonl`.
struct Hit {
    path: PathBuf,
    /// The file name: the stem-identity key across project dirs.
    name: String,
}

/// Every `<projects>/*/<id>*.jsonl` transcript, sorted by path. A dotted stem
/// is a sibling artifact (`<uuid>.orphaned-...`), never a transcript.
fn store_hits(projects_root: &Path, id: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects_root) else {
        return hits;
    };
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name_os = file.file_name();
            let Some(name) = name_os.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !name.starts_with(id) || stem.contains('.') {
                continue;
            }
            hits.push(Hit {
                path: file.path(),
                name: name.to_string(),
            });
        }
    }
    hits.sort_by(|a, b| a.path.cmp(&b.path));
    hits
}

/// True iff the transcript holds at least one user/assistant turn: a real
/// conversation, not a metadata-only stub. Short-circuits on the first
/// conversational record, so a multi-MB transcript costs a few KB. A file we
/// cannot read is not proof of conversation.
fn has_conversation(path: &Path) -> bool {
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    for line in std::io::BufRead::lines(std::io::BufReader::new(f)) {
        let Ok(line) = line else {
            return false;
        };
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if matches!(
            rec.get("type").and_then(|t| t.as_str()),
            Some("user") | Some("assistant")
        ) {
            return true;
        }
    }
    false
}

/// The newest-mtime hit, statting defensively so a file vanishing mid-scan
/// never sinks the resolution. `None` only when every stat failed.
fn newest<'a>(hits: impl Iterator<Item = &'a Hit>) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, &Hit)> = None;
    for hit in hits {
        let Ok(mt) = std::fs::metadata(&hit.path).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map_or(true, |(b, _)| mt > *b) {
            best = Some((mt, hit));
        }
    }
    best.map(|(_, h)| h.path.clone())
}

/// One id's transcript: the store walk's best answer, or `None`.
fn resolve_one(projects_root: &Path, id: &str) -> Option<PathBuf> {
    // An empty id matches every transcript (the ambiguous branch would hand
    // back the store's first file); it reads missing-input, never an answer.
    if id.is_empty() {
        return None;
    }
    let hits = store_hits(projects_root, id);
    let first = hits.first()?;
    if hits.iter().any(|h| h.name != first.name) {
        // A short prefix matched two DISTINCT session uuids across the store:
        // genuinely ambiguous. The first sorted wins, never a guess across
        // two sessions.
        return Some(first.path.clone());
    }
    if hits.len() == 1 {
        return Some(first.path.clone());
    }
    // One session copied across canonical + worktree dirs (EnterWorktree
    // re-keys the transcript; CC leaves a stub in the other dir). Mtime alone
    // is a trap: the stub's creation can post-date the real transcript's
    // last turn. Prefer copies that actually carry conversation, newest write
    // among those; all-stubs falls back to newest of all.
    let conversational = hits.iter().filter(|h| has_conversation(&h.path));
    newest(conversational).or_else(|| newest(hits.iter()))
}

/// The provider-cap sweep's claude bridge: every non-codex row's id answered
/// in one pass, keyed by the id the row itself carries. A resolved id maps to
/// its transcript path; an unresolved id is absent (the snapshot reads the
/// named unknown). The `Result` keeps the sweep's failure contract: a broken
/// resolver reads a named unknown, never a false "fine".
pub fn claude_transcript_paths(projects_root: PathBuf) -> ClaudePaths {
    Box::new(move |ids: &[String]| {
        let mut out = BTreeMap::new();
        for id in ids {
            if let Some(path) = resolve_one(&projects_root, id) {
                out.insert(id.clone(), path.to_string_lossy().to_string());
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "claude-transcript-paths-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const CONVO: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#,
    );
    const STUB: &str = r#"{"type":"summary","summary":"a metadata-only stub"}"#;

    fn plant(root: &Path, dir: &str, name: &str, body: &str) -> PathBuf {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn full_uuid_and_short_prefix_resolve_across_project_dirs() {
        let root = tmp("uuid");
        let uuid = "d8996f9b-8854-4f22-8c28-c7819c6d0316";
        plant(&root, "-repo", &format!("{uuid}.jsonl"), CONVO);
        // A worktree copy of the SAME session resolves to the same id; the
        // worktree copy is the newest write, so it wins the shared-name rule.
        let worktree = plant(&root, "-repo-worktrees-x", &format!("{uuid}.jsonl"), CONVO);
        assert_eq!(resolve_one(&root, uuid).unwrap(), worktree);
        assert_eq!(resolve_one(&root, "d8996f9b").unwrap(), worktree);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dotted_stem_is_a_sibling_artifact_never_a_transcript() {
        let root = tmp("artifact");
        let uuid = "d8996f9b-8854-4f22-8c28-c7819c6d0316";
        plant(
            &root,
            "-repo",
            &format!("{uuid}.orphaned-copy.jsonl"),
            CONVO,
        );
        assert_eq!(resolve_one(&root, uuid), None);
        assert_eq!(resolve_one(&root, "d8996f9b"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_conversational_copy_outranks_a_newer_stub() {
        let root = tmp("stub");
        let uuid = "d8996f9b-8854-4f22-8c28-c7819c6d0316";
        let real = plant(&root, "-repo", &format!("{uuid}.jsonl"), CONVO);
        // The stub is written AFTER the real transcript, so newest-mtime
        // alone would pick it.
        let stub = plant(&root, "-repo-wt", &format!("{uuid}.jsonl"), STUB);
        assert!(
            stub.metadata().unwrap().modified().unwrap()
                > real.metadata().unwrap().modified().unwrap()
        );
        assert_eq!(resolve_one(&root, uuid).unwrap(), real);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn all_stub_copies_fall_back_to_newest() {
        let root = tmp("allstub");
        let uuid = "d8996f9b-8854-4f22-8c28-c7819c6d0316";
        plant(&root, "-repo", &format!("{uuid}.jsonl"), STUB);
        // Planted second, so it is the newest write: the fallback's answer.
        let second = plant(&root, "-repo-wt", &format!("{uuid}.jsonl"), STUB);
        assert_eq!(resolve_one(&root, uuid).unwrap(), second);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_ambiguous_prefix_picks_the_first_sorted_never_a_guess() {
        let root = tmp("ambiguous");
        let one = plant(&root, "-repo", "d8996f9b-1111.jsonl", CONVO);
        plant(&root, "-repo", "d8996f9b-2222.jsonl", CONVO);
        assert_eq!(resolve_one(&root, "d8996f9b").unwrap(), one);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_store_answers_none_and_the_bridge_omits_unresolved_ids() {
        let root = tmp("missing");
        assert_eq!(resolve_one(&root.join("nope"), "d8996f9b"), None);
        let projects = root.join("projects");
        plant(
            &projects,
            "-repo",
            "d8996f9b-8854-4f22-8c28-c7819c6d0316.jsonl",
            CONVO,
        );
        // An empty id would match every transcript; it answers None.
        assert_eq!(resolve_one(&projects, ""), None);
        let paths = claude_transcript_paths(projects.clone());
        let out = paths(&["d8996f9b".to_string(), "ffffffff".to_string()]).unwrap();
        assert_eq!(out.len(), 1, "only the resolved id is present: {out:?}");
        assert!(out["d8996f9b"].ends_with("d8996f9b-8854-4f22-8c28-c7819c6d0316.jsonl"));
        assert!(paths(&[]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}

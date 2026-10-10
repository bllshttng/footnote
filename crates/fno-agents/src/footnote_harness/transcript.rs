//! Where footnote sessions live, as the supervisor resolves it. The record
//! format and its reader are `crate::footnote_transcript`, shared with the
//! `footnote` binary; the writer lives in `crates/footnote`.

use std::path::{Path, PathBuf};

pub use crate::footnote_transcript::{
    is_record_file, read_records, transcript_file, KNOWN_TYPES, SCHEMA_V,
};

/// `~/.fno/sessions`, beside the spaces root so test fences on that root
/// cover it too.
pub fn sessions_root() -> PathBuf {
    let spaces = crate::paths::spaces_root();
    spaces
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(spaces)
        .join("sessions")
}

/// The project key: the canonical checkout's space slug, so every worktree
/// of a repo shares one; `_none` outside a repo.
pub fn project_slug(cwd: &Path) -> String {
    crate::paths::canonical_repo_root(cwd)
        .map(|root| crate::paths::space_slug(&root))
        .unwrap_or_else(|| "_none".to_string())
}

pub fn session_dir(root: &Path, cwd: &Path, fno_id: &str) -> PathBuf {
    root.join(project_slug(cwd)).join(fno_id)
}

/// Find a session's directory under any project slug: the sidecar dir
/// whose record `<fno_id>.jsonl` exists beside it.
pub fn find_session_dir(root: &Path, fno_id: &str) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join(fno_id))
        .find(|p| transcript_file(p, fno_id).is_file())
}

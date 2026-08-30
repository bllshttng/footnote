//! Shared helpers for the review-coverage integration tests. A directory
//! module, not a test target of its own.

use std::fs;
use std::path::{Path, PathBuf};

/// Write an executable stub script.
///
/// Published atomically: the body is written to a temp sibling, chmod'd,
/// then renamed onto the final path (same fix as tests/loop_check.rs). The
/// published path is complete and closed from birth, so an exec can never
/// hit ETXTBSY (needs a write-open fd on the inode, including via a fork
/// that inherited one) or a partial file - no probe-exec loop required.
pub fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

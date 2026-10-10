//! The download leg of `fno update`: install the release tarball CI built
//! for the source's crates/ rev, so an update never compiles when CI already
//! did. `main-binaries.yml` builds one tarball per platform on each main merge
//! that touches crates/ and uploads it to the rolling `bin-cache` pre-release
//! as `fno-bin-<crates_rev>-<platform>.tar.gz`, beside a `.sha256` file.
//!
//! Keyed by the crates/ subtree rev, not HEAD: that is the rev the binaries
//! bake in and the rev the freshness verdict compares, so a Python-only merge
//! reuses the last tarball instead of waiting on a build.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use sha2::{Digest, Sha256};

pub(crate) const BIN_CACHE_TAG: &str = "bin-cache";
const DEFAULT_REPO: &str = "bllshttng/footnote";

/// The GitHub `owner/repo` releases publish to; the env override keeps a
/// fork's install testable.
pub(crate) fn release_repo() -> String {
    std::env::var("FNO_RELEASE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string())
}
/// One curl bound per file. The tarball is about 30 MB; a link that cannot
/// move it in this window loses to the compile fallback anyway.
const FETCH_SECS: &str = "45";

/// The four binaries every tarball carries: the fno-agents triad plus the
/// `fno` front door.
pub(crate) const BINARIES: [&str; 4] = [
    "fno-agents",
    "fno-agents-daemon",
    "fno-agents-worker",
    "fno",
];

/// The CI matrix name for this host, or None where CI builds no tarball.
pub(crate) fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("macos-arm64"),
        ("macos", "x86_64") => Some("macos-x64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

pub(crate) fn asset_name(crates_rev: &str, platform: &str) -> String {
    format!("fno-bin-{crates_rev}-{platform}.tar.gz")
}

pub(crate) fn asset_url(crates_rev: &str, platform: &str) -> String {
    let repo = release_repo();
    format!(
        "https://github.com/{repo}/releases/download/{BIN_CACHE_TAG}/{}",
        asset_name(crates_rev, platform)
    )
}

/// The hex digest from a `sha256sum`-format line (`<hex>  <name>`).
pub(crate) fn parse_sha_line(text: &str) -> Option<String> {
    let hex = text.split_whitespace().next()?.to_ascii_lowercase();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

fn curl_to(url: &str, dest: &Path) -> Result<(), String> {
    let out = crate::process_admission::std_command("curl")
        .args(["-fsSL", "--max-time", FETCH_SECS, "-o"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "curl exited {} for {url}: {}",
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Download, verify and unpack the tarball for `crates_rev` into a fresh dir
/// under `staging_parent`. Returns the dir holding the four binaries. Every
/// Err names why, so the caller's compile fallback says what it replaced. A
/// failed fetch removes its staging dir.
pub(crate) fn fetch(crates_rev: &str, staging_parent: &Path) -> Result<PathBuf, String> {
    let platform = platform().ok_or("CI builds no binary for this platform")?;
    let staging = staging_parent.join(format!("prebuilt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let fetched = fetch_into(crates_rev, platform, &staging);
    if fetched.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    fetched
}

fn fetch_into(crates_rev: &str, platform: &str, staging: &Path) -> Result<PathBuf, String> {
    let url = asset_url(crates_rev, platform);
    let tarball = staging.join(asset_name(crates_rev, platform));
    let sha_file = staging.join("tarball.sha256");
    curl_to(&format!("{url}.sha256"), &sha_file)?;
    curl_to(&url, &tarball)?;
    let want = std::fs::read_to_string(&sha_file)
        .ok()
        .as_deref()
        .and_then(parse_sha_line)
        .ok_or("the .sha256 file holds no digest")?;
    let bytes = std::fs::read(&tarball).map_err(|e| format!("{}: {e}", tarball.display()))?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != want {
        return Err(format!("sha256 mismatch: want {want}, got {got}"));
    }
    let unpacked = staging.join("bin");
    std::fs::create_dir_all(&unpacked).map_err(|e| format!("{}: {e}", unpacked.display()))?;
    let out = crate::process_admission::std_command("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&unpacked)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tar exited {}: {}",
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    proves_rev(&unpacked, crates_rev)?;
    Ok(unpacked)
}

/// The unpacked client must report `crates_rev` from a clean build before
/// anything is swapped. A tarball that would fail the post-deploy verify is
/// refused here instead, so the caller compiles rather than wedging on it.
fn proves_rev(unpacked: &Path, crates_rev: &str) -> Result<(), String> {
    let out = crate::process_admission::std_command(unpacked.join("fno-agents"))
        .args(["version", "--json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("the downloaded fno-agents did not run: {e}"))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("the downloaded fno-agents version --json is not JSON: {e}"))?;
    let baked = v.get("crates_rev").and_then(serde_json::Value::as_str);
    let dirty = v.get("dirty").and_then(serde_json::Value::as_bool);
    if baked == Some(crates_rev) && dirty == Some(false) {
        Ok(())
    } else {
        Err(format!(
            "the downloaded fno-agents reports crates_rev {} dirty {}, want {crates_rev} clean",
            baked.unwrap_or("none"),
            dirty.map_or("unknown".to_string(), |d| d.to_string())
        ))
    }
}

/// Move the four unpacked binaries into `bin_dir`. Every copy lands first as
/// a temp file beside its target, and only then do the renames run, so a
/// failed copy leaves the old set whole. A rename over a running binary is
/// safe on unix: the live process keeps its old inode.
pub(crate) fn swap_into(unpacked: &Path, bin_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let missing: Vec<&str> = BINARIES
        .iter()
        .copied()
        .filter(|n| !unpacked.join(n).is_file())
        .collect();
    if !missing.is_empty() {
        return Err(format!("the tarball lacks {}", missing.join(", ")));
    }
    std::fs::create_dir_all(bin_dir).map_err(|e| format!("{}: {e}", bin_dir.display()))?;
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for name in BINARIES {
        let tmp = bin_dir.join(format!(".{name}.prebuilt-{}", std::process::id()));
        let copied = std::fs::copy(unpacked.join(name), &tmp)
            .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)));
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&tmp);
            for (t, _) in &staged {
                let _ = std::fs::remove_file(t);
            }
            return Err(format!("{}: {e}", tmp.display()));
        }
        staged.push((tmp, bin_dir.join(name)));
    }
    for (tmp, dest) in &staged {
        std::fs::rename(tmp, dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_key_on_crates_rev_and_platform() {
        assert_eq!(
            asset_name("abc123", "macos-arm64"),
            "fno-bin-abc123-macos-arm64.tar.gz"
        );
        assert!(asset_url("abc123", "linux-x64")
            .ends_with("/releases/download/bin-cache/fno-bin-abc123-linux-x64.tar.gz"));
        let hex = "a".repeat(64);
        assert_eq!(parse_sha_line(&format!("{hex}  x.tar.gz\n")), Some(hex));
        assert_eq!(parse_sha_line("not-a-digest  x.tar.gz"), None);
    }

    #[test]
    fn swap_moves_all_four_or_none() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        for name in &BINARIES[..3] {
            std::fs::write(src.path().join(name), b"new").unwrap();
        }
        std::fs::write(dest.path().join("fno-agents"), b"old").unwrap();
        let err = swap_into(src.path(), dest.path()).unwrap_err();
        assert!(err.contains("lacks fno"), "{err}");
        assert_eq!(
            std::fs::read(dest.path().join("fno-agents")).unwrap(),
            b"old"
        );

        std::fs::write(src.path().join("fno"), b"new").unwrap();
        swap_into(src.path(), dest.path()).unwrap();
        for name in BINARIES {
            assert_eq!(std::fs::read(dest.path().join(name)).unwrap(), b"new");
        }
        let leftovers = std::fs::read_dir(dest.path()).unwrap().count();
        assert_eq!(leftovers, BINARIES.len());
    }
}

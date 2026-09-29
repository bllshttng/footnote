//! Provider API-key resolution: the env var named by `api_key_env`, else the
//! same name read from the `api_key_file` dotenv file.
//!
//! The one key rule both crates need (the codex route resolver and the mux
//! composer's launch check). `crates/fno/src/provider_key.rs` is a generated
//! copy of this file, so the two crates cannot drift; `crates/fno` does not
//! depend on `fno-agents` (independent crates.io publishes).

use std::path::{Path, PathBuf};

/// Key precedence (twin of `_resolve_key`, `model_routing.py`): the env var
/// named by `api_key_env` wins over the same name read from the
/// `api_key_file` dotenv file. Never returns the empty string.
pub fn resolve_key(key_env: &str, api_key_file: Option<&str>) -> Option<String> {
    if key_env.is_empty() {
        return None;
    }
    if let Ok(from_env) = std::env::var(key_env) {
        if !from_env.is_empty() {
            return Some(from_env);
        }
    }
    api_key_file.and_then(|file| read_var_from_env_file(file, key_env))
}

/// Whether a key resolves, without exposing it: the composer's launch check
/// and any other presence-only caller read through here.
pub fn key_present(key_env: &str, api_key_file: Option<&str>) -> bool {
    resolve_key(key_env, api_key_file).is_some()
}

/// Port of Python's `read_var_from_env_file` (`cli/src/fno/env_file.py`):
/// skip `#` lines, allow an `export ` prefix, trim, strip the quotes Python's
/// `str.strip` would strip. A missing file or key is None, never fatal.
fn read_var_from_env_file(path_str: &str, key_name: &str) -> Option<String> {
    let path = expand_tilde(path_str)?;
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key_name {
            let v = py_strip(value.trim(), '"');
            let v = py_strip(v, '\'');
            return if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
    }
    None
}

/// `str.strip(char)`: repeatedly drop the quote from both ends.
fn py_strip(s: &str, quote: char) -> &str {
    let mut s = s;
    while let Some(rest) = s.strip_prefix(quote) {
        s = rest;
    }
    while let Some(rest) = s.strip_suffix(quote) {
        s = rest;
    }
    s
}

fn expand_tilde(path_str: &str) -> Option<PathBuf> {
    if path_str == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    match path_str.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| Path::new(&home).join(rest)),
        None => Some(PathBuf::from(path_str)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "FNO_TEST_PROVIDER_KEY";

    /// Serializes tests that touch the process env (key precedence reads the
    /// real env vars) and hands each test a fresh temp dir.
    fn env_guard() -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        static LOCK: std::sync::LazyLock<&'static std::sync::Mutex<()>> =
            std::sync::LazyLock::new(crate::claims::test_env_lock);
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-provider-key-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (dir, guard)
    }

    #[test]
    fn key_precedence_env_beats_file() {
        let (dir, _guard) = env_guard();
        let file = dir.join("keys.env");
        std::fs::write(&file, format!("{KEY}=from-file\n")).unwrap();
        let path = file.to_str().unwrap();
        std::env::remove_var(KEY);
        assert_eq!(resolve_key(KEY, Some(path)), Some("from-file".to_string()));
        std::env::set_var(KEY, "from-env");
        assert_eq!(resolve_key(KEY, Some(path)), Some("from-env".to_string()));
        // An empty env value falls through to the file, like Python's read.
        std::env::set_var(KEY, "");
        assert_eq!(resolve_key(KEY, Some(path)), Some("from-file".to_string()));
        std::env::remove_var(KEY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_file_reader_matches_python() {
        let (dir, _guard) = env_guard();
        let f = dir.join("keys.env");
        std::fs::write(
            &f,
            "# comment\nexport K1 = 'v one'\nK2=\"v2\"\nK3=\nK1=overwritten-later-line-wins-no-first-wins\n",
        )
        .unwrap();
        // First match wins, like Python's line loop.
        assert_eq!(
            read_var_from_env_file(f.to_str().unwrap(), "K1"),
            Some("v one".to_string())
        );
        assert_eq!(
            read_var_from_env_file(f.to_str().unwrap(), "K2"),
            Some("v2".to_string())
        );
        assert_eq!(read_var_from_env_file(f.to_str().unwrap(), "K3"), None);
        assert_eq!(read_var_from_env_file(f.to_str().unwrap(), "K4"), None);
        assert_eq!(read_var_from_env_file("/nonexistent/env", "K1"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_present_never_leaks_and_handles_the_empty_env_name() {
        let (dir, _guard) = env_guard();
        std::env::set_var(KEY, "present");
        assert!(key_present(KEY, None));
        std::env::remove_var(KEY);
        assert!(!key_present(KEY, None));
        let file = dir.join("keys.env");
        std::fs::write(&file, format!("{KEY}=from-file\n")).unwrap();
        assert!(key_present(KEY, Some(file.to_str().unwrap())));
        assert!(!key_present("", Some(file.to_str().unwrap())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

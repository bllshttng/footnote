//! Which root the mux web bridge serves the lead page from: the
//! PROJECT-AWARE `state_dir` ladder Python's renderer resolves, plus the
//! faithfulness verdict the /team route needs before it may write.

/// The state root Python's renderer writes `lead.html` to, plus whether the
/// resolution is FAITHFUL: the PROJECT-AWARE `state_dir` ladder (Python
/// `fno.paths.state_dir()`), not the mux's explicit-only tier. Writer and
/// reader must walk the same chain or /team serves the global dir's stale
/// page while the project render lands elsewhere. A miss is faithful (both
/// sides land on Python's `~/.fno` default); a DECLINED value - a form the
/// mirror refuses to duplicate, like a `{vault}`/`{project}` template, a
/// `~user` or relative anchor, or an unset `$VAR` - falls back to
/// `legacy_state_root()` with `false`: the caller must never WRITE through
/// an unfaithful root, or the /team refresh overwrites the global page
/// with this project's org data.
#[cfg(not(test))]
pub(crate) fn lead_state_root() -> (std::path::PathBuf, bool) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    match crate::digest_overlay::config_top_str(&cwd, "state_dir") {
        Some(raw) => match expand_lead_state_dir(&raw) {
            Some(root) => (root, true),
            None => {
                crate::proto::warn_once_unexpandable_state_dir(&raw);
                (crate::proto::legacy_state_root(), false)
            }
        },
        None => {
            crate::proto::warn_once_legacy_yaml_state_dir();
            (crate::proto::legacy_state_root(), true)
        }
    }
}

/// Expand a state_dir the way Python's renderer does for the lead page: the
/// expandvars pass FIRST (see [`expand_lead_state_vars`]), then the shared
/// `~`/absolute expansion. `None` still means declined: a form this crate
/// does not duplicate (`{vault}`/`{project}` templates, `~user`, relative
/// anchors, an unset variable). The caller must treat the declined root as
/// unfaithful and never write through it.
pub(crate) fn expand_lead_state_dir(raw: &str) -> Option<std::path::PathBuf> {
    let expanded = expand_lead_state_vars(raw.trim());
    crate::proto::expand_state_dir(&expanded)
}

/// The `$VAR`/`${VAR}` pass Python's `_resolve` runs FIRST on a state_dir,
/// mirroring `os.path.expandvars`: a set variable substitutes (an empty
/// value allowed), an UNSET one stays literal so the result keeps its `$`
/// and the shared expansion declines it instead of guessing a root.
/// Lead-only: the mux's own expansion deliberately declines `$` forms
/// (see `crate::proto::expand_state_dir`).
pub(crate) fn expand_lead_state_vars(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch != '$' || i + 1 >= raw.len() {
            out.push(ch);
            continue;
        }
        let rest = &raw[i + 1..];
        let (name, consumed) = if let Some(inner) = rest.strip_prefix('{') {
            match inner.find('}') {
                Some(end) => (Some(&inner[..end]), inner[..end].chars().count() + 2),
                None => (None, 0),
            }
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (Some(&rest[..end]), end)
        };
        match name.and_then(|n| std::env::var(n).ok()) {
            Some(value) => {
                out.push_str(&value);
                for _ in 0..consumed {
                    chars.next();
                }
            }
            None => out.push('$'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn lead_expansion_resolves_env_vars_python_order() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_TEST_STATE_BASE", "/srv/fno-state");
        assert_eq!(
            expand_lead_state_dir("$FNO_TEST_STATE_BASE"),
            Some(PathBuf::from("/srv/fno-state"))
        );
        assert_eq!(
            expand_lead_state_dir("${FNO_TEST_STATE_BASE}/lead"),
            Some(PathBuf::from("/srv/fno-state/lead"))
        );
        // A set-but-empty variable substitutes to the empty string, exactly
        // like os.path.expandvars: the remainder reads as its own root.
        std::env::set_var("FNO_TEST_STATE_BASE", "");
        assert_eq!(
            expand_lead_state_dir("$FNO_TEST_STATE_BASE/x"),
            Some(PathBuf::from("/x"))
        );
        std::env::remove_var("FNO_TEST_STATE_BASE");
        // An UNSET variable stays literal, so the value keeps its `$` and
        // declines instead of guessing a root.
        assert_eq!(expand_lead_state_dir("$FNO_TEST_STATE_BASE/x"), None);
    }

    #[test]
    fn lead_expansion_still_declines_owner_owned_forms() {
        // Templates, ~user and relative anchors stay declined: this crate
        // does not duplicate their owner.
        assert_eq!(expand_lead_state_dir("{vault}/state"), None);
        assert_eq!(expand_lead_state_dir("{project}/state"), None);
        assert_eq!(expand_lead_state_dir("~user/state"), None);
        assert_eq!(expand_lead_state_dir("relative/state"), None);
        // Absolute and ~-rooted values pass through untouched.
        assert_eq!(
            expand_lead_state_dir("/abs/state"),
            Some(PathBuf::from("/abs/state"))
        );
    }
}

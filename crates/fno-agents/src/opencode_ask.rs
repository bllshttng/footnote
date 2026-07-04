//! Client interceptor for the `ask` (resume-by-name) verb on an opencode
//! target (x-51f6). v1 hosts opencode as a pane only — there is no client-side
//! headless dispatch and no stateful resume — so `ask` against an existing
//! opencode row must refuse with a message naming the real limitation, not
//! fall through to `bin/client.rs`'s `unresolvable_ask_exit` "provider is
//! required for new agent" text (which is both wrong, the agent already
//! exists, and a dead end, the caller's own suggested retry hits the same
//! message). Mirrors [`crate::agy_ask::maybe_run_agy_ask`]'s shape.

/// Returns `None` for a non-opencode target (fall through to the next
/// provider's ask hook), or `Some(2)` after printing the refusal.
pub fn maybe_run_opencode_ask(
    home: &crate::paths::AgentsHome,
    params: &serde_json::Value,
    name: &str,
) -> Option<i32> {
    let provider_param = params.get("provider").and_then(|v| v.as_str());
    let registry = match crate::state::load_registry(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "fno-agents: cannot read agents registry at {:?}: {}",
                home.registry_json(),
                e
            );
            return Some(12);
        }
    };
    let existing_provider = registry.find(name).map(|e| e.provider.as_str());
    let resolved = existing_provider.or(provider_param);
    if resolved != Some("opencode") {
        return None; // not an opencode target; fall through
    }
    eprintln!(
        "fno-agents: opencode has no stateful 'ask' resume (pane-hosted, no client-side dispatch); \
         drive the pane directly with 'fno mux pane send <session> <pane> --text <prompt>'."
    );
    Some(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(dir: &std::path::Path) -> crate::paths::AgentsHome {
        crate::paths::AgentsHome::at(dir.to_path_buf())
    }

    // Raw JSON (not a typed RegistryEntry) so this test only names the fields
    // it cares about; every other field has a `#[serde(default)]` on the real
    // struct and deserializes fine without them.
    fn write_registry_row(home: &crate::paths::AgentsHome, name: &str, provider: &str) {
        std::fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": [{
                "name": name,
                "provider": provider,
                "cwd": "/x",
                "status": "live",
                "created_at": "2026-01-01T00:00:00Z",
            }],
        });
        std::fs::write(home.registry_json(), body.to_string()).unwrap();
    }

    fn write_empty_registry(home: &crate::paths::AgentsHome) {
        std::fs::create_dir_all(home.registry_json().parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "agents": [],
        });
        std::fs::write(home.registry_json(), body.to_string()).unwrap();
    }

    #[test]
    fn non_opencode_target_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        write_empty_registry(&h);
        let params = serde_json::json!({"provider": "codex"});
        assert_eq!(maybe_run_opencode_ask(&h, &params, "wk"), None);
    }

    #[test]
    fn existing_opencode_row_refuses_by_registry_lookup_alone() {
        // No --provider flag needed: the registry lookup resolves it, exactly
        // like the "agent already exists" case the finding named.
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        write_registry_row(&h, "oc", "opencode");
        let params = serde_json::json!({});
        assert_eq!(maybe_run_opencode_ask(&h, &params, "oc"), Some(2));
    }

    #[test]
    fn provider_flag_alone_also_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        write_empty_registry(&h);
        let params = serde_json::json!({"provider": "opencode"});
        assert_eq!(maybe_run_opencode_ask(&h, &params, "new-oc"), Some(2));
    }
}

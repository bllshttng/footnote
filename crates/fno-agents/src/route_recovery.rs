//! The route a bare row's session launched on, recovered from evidence.
//!
//! A registry row that records no model, provider or route (an adopted row,
//! or one whose fields never healed) would resume on the account default.
//! The transcript's birth identity names the model the session was born to
//! serve; the recorded route settings name the provider and carry the route
//! file. Recovery answers with that route or refuses by name - never a
//! default-endpoint launch.

use std::path::{Path, PathBuf};

/// The route a bare row's session launched on, ready to record and to ride
/// the relaunch argv as `--settings`.
#[derive(Debug, Clone)]
pub struct Recovered {
    pub provider: String,
    pub model: String,
    pub route_settings_path: String,
}

/// The newest usable recorded route file for `provider` + `model`.
///
/// A file qualifies when its env names the model, its stamp names the
/// provider, it carries no top-level `sandbox` block (a sandbox names one
/// spawn's worktree paths) and [`crate::reentry::validate_route_settings`]
/// still accepts it. Newest by mtime wins - a content-addressed file is
/// written once, so its mtime is when that composition first appeared - and
/// ties break by the greater file name so the pick is deterministic.
pub fn route_file_for(provider: &str, model: &str) -> Option<PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for (path, v) in crate::claude_adopt::route_settings_files() {
        let Some(env) = v.get("env").and_then(|e| e.as_object()) else {
            continue;
        };
        if env.get("ANTHROPIC_MODEL").and_then(|m| m.as_str()) != Some(model) {
            continue;
        }
        if env.get("FNO_ROUTE_PROVIDER").and_then(|p| p.as_str()) != Some(provider) {
            continue;
        }
        if v.get("sandbox").is_some() {
            continue;
        }
        let path_str = path.to_string_lossy().to_string();
        if crate::reentry::validate_route_settings(&path_str).is_err() {
            continue;
        }
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((mtime, path));
    }
    pick_newest(candidates)
}

/// Newest by mtime, ties by the greater path. Pure so the tie-break is
/// testable without mtime surgery.
fn pick_newest(candidates: Vec<(std::time::SystemTime, PathBuf)>) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for (mtime, path) in candidates {
        let better = match &best {
            None => true,
            Some((bt, bp)) => mtime > *bt || (mtime == *bt && path > *bp),
        };
        if better {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Which recorded route did this bare row's session launch on?
///
/// `Ok(None)` means nothing to restore: the session resolves on its own
/// evidence (an Anthropic birth, no transcript, or a row that already
/// answers). `Err` is the refusal text resume_pin named - a route the
/// recorded files do not serve, so the relaunch refuses instead of guessing.
pub fn recover(
    pins: crate::resume_pin::RowPins,
    session_id: &str,
    projects: &Path,
) -> Result<Option<Recovered>, String> {
    let transcript = crate::claude_drive::find_transcript_in(projects, session_id);
    let lookup: crate::resume_pin::RouteProviderOf<'_> =
        &|m| crate::claude_adopt::provider_from_route_settings(m);
    match crate::resume_pin::resolve(Some(pins), transcript.as_deref(), false, session_id, lookup) {
        Ok(_) => Ok(None),
        Err(u) if u.lost_route.is_some() => {
            let (p, m) = u.lost_route.expect("lost_route checked above");
            match route_file_for(&p, &m) {
                Some(path) => Ok(Some(Recovered {
                    provider: p,
                    model: m,
                    route_settings_path: path.to_string_lossy().to_string(),
                })),
                None => Err(u.text),
            }
        }
        // A model the evidence names but no route file serves: refuse here
        // too - the relaunch must not land on the account default.
        Err(u) if u.model.is_some() => Err(u.text),
        // Nothing established the model at all: today's behavior, no answer.
        Err(_) => Ok(None),
    }
}

/// Write a recovered route onto the row the session id names, filling ONLY
/// empty fields. The session id is the key, never the row name, and the
/// running `model` is never stored - the birth model is the spawn's request,
/// which is what `requested_model` records. `launch_account` becomes
/// `"default"` because the transcript was found under the default root.
/// Returns whether anything changed.
pub fn persist(
    registry_path: &Path,
    session_id: &str,
    r: &Recovered,
) -> Result<bool, crate::state::StateError> {
    crate::state::update_registry(registry_path, |reg| {
        let is_blank = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
        let Some(entry) = reg.entries.iter_mut().find(|e| {
            e.harness_session_id.as_deref() == Some(session_id)
                || e.claude_session_uuid.as_deref() == Some(session_id)
                || e.related_session_id.as_deref() == Some(session_id)
        }) else {
            return false;
        };
        let mut changed = false;
        let mut fill = |slot: &mut Option<String>, value: String| {
            if is_blank(slot) {
                *slot = Some(value);
                changed = true;
            }
        };
        fill(
            &mut entry.route_settings_path,
            r.route_settings_path.clone(),
        );
        fill(&mut entry.provider, r.provider.clone());
        fill(&mut entry.requested_model, r.model.clone());
        fill(&mut entry.launch_account, "default".to_string());
        changed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_route_dir(tag: &str, files: &[(&str, serde_json::Value)]) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "fno-route-rec-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (i, (name, payload)) in files.iter().enumerate() {
            std::fs::write(
                base.join(format!("{i:04}-{name}.json")),
                payload.to_string(),
            )
            .unwrap();
        }
        base
    }

    fn route_env(model: &str, provider: &str, sandboxed: bool, floor: bool) -> serde_json::Value {
        let mut env = serde_json::json!({
            "ANTHROPIC_MODEL": model,
            "FNO_ROUTE_PROVIDER": provider,
        });
        if floor {
            env["ANTHROPIC_API_KEY"] = "".into();
            env["ANTHROPIC_BASE_URL"] = "".into();
        } else {
            env["ANTHROPIC_BASE_URL"] = "https://repro.invalid/api/anthropic".into();
            env["ANTHROPIC_AUTH_TOKEN"] = "secret-token".into();
        }
        let mut v = serde_json::json!({ "env": env });
        if sandboxed {
            v["sandbox"] = serde_json::json!({"workspace": "/some/worktree"});
        }
        v
    }

    #[test]
    fn pick_newest_breaks_ties_by_the_greater_path() {
        let t = std::time::UNIX_EPOCH;
        let cands = vec![
            (t, PathBuf::from("/r/a.json")),
            (t, PathBuf::from("/r/b.json")),
        ];
        assert_eq!(pick_newest(cands), Some(PathBuf::from("/r/b.json")));
        let later = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        let cands = vec![
            (t, PathBuf::from("/r/b.json")),
            (later, PathBuf::from("/r/a.json")),
        ];
        assert_eq!(pick_newest(cands), Some(PathBuf::from("/r/a.json")));
    }

    #[test]
    fn route_file_for_skips_sandboxed_and_breaks_ties() {
        // AC2-HP: among a sandboxed file and two sandbox-free zai files for
        // the same model, a sandbox-free file answers, and the same one on
        // every run.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = seed_route_dir(
            "tie",
            &[
                ("sand", route_env("glm-5.3-flash[1m]", "zai", true, false)),
                ("one", route_env("glm-5.3-flash[1m]", "zai", false, false)),
                ("two", route_env("glm-5.3-flash[1m]", "zai", false, false)),
            ],
        );
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &base);
        let first = route_file_for("zai", "glm-5.3-flash[1m]");
        let second = route_file_for("zai", "glm-5.3-flash[1m]");
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
        let picked = first.expect("a sandbox-free file answers");
        assert_eq!(Some(picked.clone()), second);
        assert!(!picked.to_string_lossy().contains("0000-sand"));
        // The pick is one of the sandbox-free pair, whichever mtime won.
        assert!(picked.to_string_lossy().ends_with(".json"));
    }

    #[test]
    fn route_file_for_misses_on_sandbox_or_floor_only() {
        // AC2-ERR: only sandboxed files, or only the empty scrub floor:
        // nothing usable answers.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = seed_route_dir(
            "sandbox-only",
            &[("sand", route_env("glm-5.3-flash[1m]", "zai", true, false))],
        );
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &base);
        let missed = route_file_for("zai", "glm-5.3-flash[1m]");
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
        assert_eq!(missed, None);

        // A scrub-floor file that does not even name the model matches
        // nothing. (One that DID name the model carries non-empty route
        // evidence by construction, so validate passes it - the model name
        // is exactly what this lookup searched for.)
        let base = seed_route_dir(
            "floor-only",
            &[("floor", route_env("claude-opus-5", "zai", false, true))],
        );
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &base);
        let missed = route_file_for("zai", "glm-5.3-flash[1m]");
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
        assert_eq!(missed, None);
    }

    fn bare_row_transcript(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "route-rec-{tag}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"attachment","attachment":{{"type":"model","identity":{{"modelId":"glm-5.3-flash[1m]","marketingName":null}}}},"type":"attachment"}}"#
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn recover_refuses_with_the_recipe_when_no_usable_route_answers() {
        // AC2-ERR: a bare glm-born row over only a sandboxed route file
        // refuses with the spawn --resume recipe; recovery never guesses.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = seed_route_dir(
            "recover-sand",
            &[("sand", route_env("glm-5.3-flash[1m]", "zai", true, false))],
        );
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &base);
        let projects =
            std::env::temp_dir().join(format!("route-rec-projects-{}", std::process::id()));
        let slug = projects.join("slug-proj");
        std::fs::create_dir_all(&slug).unwrap();
        let transcript = bare_row_transcript("recover-err");
        std::fs::copy(
            &transcript,
            slug.join("77770005-2222-3333-4444-555555555555.jsonl"),
        )
        .unwrap();
        let answer = recover(
            crate::resume_pin::RowPins::default(),
            "77770005-2222-3333-4444-555555555555",
            &projects,
        );
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&projects).ok();
        std::fs::remove_file(&transcript).ok();
        let text = answer.expect_err("no usable route refuses");
        assert!(text.contains("fno agents spawn --resume"), "{text}");
    }

    #[test]
    fn persist_fills_only_empty_fields() {
        // AC2-EDGE: a row that already records a provider keeps it; only the
        // empty fields fill, and `model` is never written.
        let _root = crate::paths::DeclaredRoot::declare("persist_fills_only_empty_fields");
        let dir = std::env::temp_dir().join(format!(
            "route-rec-reg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        let sid = "77770006-2222-3333-4444-555555555555";
        crate::state::update_registry(&reg, |r| {
            r.entries.push(crate::state::RegistryEntry {
                name: "wk-test".into(),
                harness: Some("claude".into()),
                harness_session_id: Some(sid.into()),
                provider: Some("anthropic".into()),
                model: Some("claude-opus-5".into()),
                requested_model: Some("claude-opus-5".into()),
                ..Default::default()
            });
        })
        .unwrap();
        let recovered = Recovered {
            provider: "zai".into(),
            model: "glm-5.3-flash[1m]".into(),
            route_settings_path: "/routes/zai.json".into(),
        };
        assert!(persist(&reg, sid, &recovered).unwrap());
        let loaded = crate::state::load_registry(&reg).unwrap();
        let e = &loaded.entries[0];
        assert_eq!(e.provider.as_deref(), Some("anthropic"));
        assert_eq!(e.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(e.requested_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(e.route_settings_path.as_deref(), Some("/routes/zai.json"));
        assert_eq!(e.launch_account.as_deref(), Some("default"));
        // A second run records nothing new.
        assert!(!persist(&reg, sid, &recovered).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}

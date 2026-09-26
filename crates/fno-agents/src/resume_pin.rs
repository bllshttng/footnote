//! Which model a claude resume comes back on.
//!
//! claude launches a resume on the account default unless the argv names a
//! model, so every fno door that resumes without the job's saved launch asks
//! this module first: pin the argv, or refuse by name.

use serde_json::{json, Value};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// The placeholder a transcript's synthetic turns carry instead of a model.
pub const SYNTHETIC_MODEL: &str = "<synthetic>";

/// The model axes a registry row states, plus the provider the row records.
#[derive(Debug, Clone, Default)]
pub struct RowPins {
    pub requested_model: Option<String>,
    pub model: Option<String>,
    pub requested_effort: Option<String>,
    pub effort: Option<String>,
    pub provider: Option<String>,
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl RowPins {
    /// From the spawn-axes payload's `row` object; absent or empty keys read
    /// None, the same absence-is-unknown discipline as the row itself.
    pub fn from_json(row: &Value) -> Self {
        RowPins {
            requested_model: opt_str(row, "requested_model"),
            model: opt_str(row, "model"),
            requested_effort: opt_str(row, "requested_effort"),
            effort: opt_str(row, "effort"),
            provider: opt_str(row, "provider"),
        }
    }

    pub fn from_entry(entry: &crate::state::RegistryEntry) -> Self {
        RowPins {
            requested_model: entry.requested_model.clone(),
            model: entry.model.clone(),
            requested_effort: entry.requested_effort.clone(),
            effort: entry.effort.clone(),
            provider: entry.provider.clone(),
        }
    }
}

/// The FIRST model identity a transcript names, as (model, marketing_name).
/// A model-identity attachment answers `identity.modelId`; else the first
/// non-synthetic assistant `message.model`. Stop at the first hit; never read
/// the whole file. The LAST model is the wrong fallback here: after a bad wake
/// it reads the poisoned default, while the first identity is the birth pin.
fn birth_identity(transcript: &Path) -> (Option<String>, Option<String>) {
    let Ok(file) = std::fs::File::open(transcript) else {
        return (None, None);
    };
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("attachment") {
            if let Some(identity) = v
                .get("attachment")
                .filter(|a| a.get("type").and_then(|t| t.as_str()) == Some("model"))
                .and_then(|a| a.get("identity"))
            {
                let model = opt_str(identity, "modelId");
                let marketing = opt_str(identity, "marketingName");
                return (model, marketing);
            }
        }
        if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
            if let Some(m) = v
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty() && *m != SYNTHETIC_MODEL)
            {
                return (Some(m.to_string()), None);
            }
        }
    }
    (None, None)
}

/// The transcript's birth model, or None when neither read answers.
pub fn birth_model(transcript: &Path) -> Option<String> {
    birth_identity(transcript).0
}

/// The resolver's answer. `argv_model` is the `--model` a resume must carry
/// (None when the route owns the argv); `route_model` is the model to RECORD
/// on a routed resume; `marketing_name` rides only a transcript-sourced
/// candidate, read by the rowless unknown-provider rule.
#[derive(Debug, Clone)]
pub struct Pin {
    pub argv_model: Option<String>,
    pub route_model: Option<String>,
    pub effort: Option<String>,
    pub source: &'static str,
    pub marketing_name: Option<String>,
}

/// Why a resume cannot pin its model, and the route that would serve it.
#[derive(Debug, Clone)]
pub struct Unpinned {
    pub text: String,
    /// (provider, model) when a recorded route or the row names the provider.
    pub lost_route: Option<(String, String)>,
}

/// Which provider serves `candidate`, answered from the recorded route
/// settings. `None` is a MISS - no file names the model, or the matches
/// disagree - and the caller treats a miss as UNKNOWN, never unrouted.
pub type RouteProviderOf<'a> = &'a dyn Fn(Option<&str>) -> Option<String>;

/// Which model a resume comes back on, or the named refusal. `routed` says
/// what THIS resume restores, not how the session launched: a routed resume
/// never refuses and never pins the argv, because the route file carries
/// endpoint, auth and model as one unit.
pub fn resolve(
    row: Option<RowPins>,
    transcript: Option<&Path>,
    routed: bool,
    session_id: &str,
    route_provider_of: RouteProviderOf<'_>,
) -> Result<Pin, Unpinned> {
    let effort = row
        .as_ref()
        .and_then(|r| r.requested_effort.clone().or_else(|| r.effort.clone()));
    let (candidate, source, marketing) = match &row {
        Some(r) => (
            r.requested_model.clone().or_else(|| r.model.clone()),
            "registry",
            None,
        ),
        None => {
            let (model, marketing) = transcript.map(birth_identity).unwrap_or((None, None));
            (model, "transcript", marketing)
        }
    };
    // A routed resume never refuses: the route owns the argv, so a candidate
    // that resolves is only recorded, and no candidate records nothing.
    if routed {
        return Ok(Pin {
            argv_model: None,
            route_model: candidate,
            effort,
            source,
            marketing_name: marketing,
        });
    }

    let Some(candidate) = candidate else {
        let row_read = if row.is_some() {
            "the registry row records no model"
        } else {
            "no registry row"
        };
        let transcript_read = match transcript {
            Some(p) => format!("no model identity in {}", p.display()),
            None => "no transcript found".to_string(),
        };
        return Err(Unpinned {
            text: format!(
                "session {session_id} records no model: {row_read}; {transcript_read}; \
                 pass --model to resume it"
            ),
            lost_route: None,
        });
    };

    match route_provider_of(Some(&candidate)) {
        Some(p) if p != "anthropic" => Err(Unpinned {
            text: format!(
                "session {session_id} last ran {candidate}, which only route {p} serves, \
                 and this resume restores no route; resume it with: \
                 fno agents spawn --resume {session_id} -P {p} -m {}",
                crate::spawn_axes::repr(&candidate)
            ),
            lost_route: Some((p, candidate)),
        }),
        Some(_) => Ok(Pin {
            argv_model: Some(candidate),
            route_model: None,
            effort,
            source,
            marketing_name: marketing,
        }),
        None => {
            let row_provider = row
                .as_ref()
                .and_then(|r| r.provider.as_deref())
                .filter(|p| !p.is_empty() && *p != "anthropic");
            // Rowless: glm stamps marketingName null on its model identity;
            // Anthropic stamps "Opus 5"/"Sonnet 5". A null name means the
            // provider cannot be established, and a default-endpoint resume
            // would bill the wrong vendor.
            let rowless_unknown = row.is_none() && marketing.is_none();
            if row_provider.is_some() || rowless_unknown {
                return Err(Unpinned {
                    lost_route: row_provider.map(|p| (p.to_string(), candidate.clone())),
                    text: format!(
                        "session {session_id} last ran {candidate}, but no recorded route names \
                         its provider and this resume restores no route; resume it with: \
                         fno agents spawn --resume {session_id} -P <provider> -m {}",
                        crate::spawn_axes::repr(&candidate)
                    ),
                });
            }
            Ok(Pin {
                argv_model: Some(candidate),
                route_model: None,
                effort,
                source,
                marketing_name: marketing,
            })
        }
    }
}

/// Push `--model`/`--effort` unless the argv already names them: an explicit
/// token an earlier arm added always wins.
pub fn append_axes(argv: &mut Vec<String>, model: Option<&str>, effort: Option<&str>) {
    if !argv.iter().any(|t| t == "--model") {
        if let Some(m) = model {
            argv.push("--model".into());
            argv.push(m.to_string());
        }
    }
    if !argv.iter().any(|t| t == "--effort") {
        if let Some(e) = effort {
            argv.push("--effort".into());
            argv.push(e.to_string());
        }
    }
}

/// The spawn-axes answer for a `resume_pin` field: `{"session_id", "routed",
/// "row"}` in, the decision JSON out - `{"model", "route_model", "effort",
/// "source"}` or `{"refusal"}`.
pub fn decide(payload: &Value) -> Value {
    let session_id = payload
        .get("session_id")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let routed = payload
        .get("routed")
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    let row = payload
        .get("row")
        .filter(|r| r.is_object())
        .map(RowPins::from_json);
    let transcript: Option<PathBuf> = if session_id.is_empty() {
        None
    } else {
        crate::claude_drive::find_transcript(session_id)
    };
    let lookup: RouteProviderOf<'_> = &|m| crate::claude_adopt::provider_from_route_settings(m);
    match resolve(row, transcript.as_deref(), routed, session_id, lookup) {
        Ok(pin) => json!({
            "model": pin.argv_model,
            "route_model": pin.route_model,
            "effort": pin.effort,
            "source": pin.source,
        }),
        Err(u) => json!({
            "refusal": u.text,
            "lost_route": u.lost_route.map(|(p, m)| json!({"provider": p, "model": m})),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_transcript(tag: &str, lines: &[String]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "resume-pin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn attachment_line(model_id: &str, marketing: Option<&str>) -> String {
        let marketing = marketing
            .map(|m| format!("\"{m}\""))
            .unwrap_or_else(|| "null".to_string());
        format!(
            r#"{{"parentUuid":"75ee36ae","isSidechain":false,"attachment":{{"type":"model","identity":{{"modelId":"{model_id}","marketingName":{marketing},"knowledgeCutoff":null}},"text":"You are powered by the model {model_id}."}},"type":"attachment"}}"#
        )
    }

    fn assistant_line(model: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","model":"{model}"}},"timestamp":"2026-09-18T00:00:00Z"}}"#
        )
    }

    fn row(requested_model: Option<&str>, provider: Option<&str>) -> Option<RowPins> {
        Some(RowPins {
            requested_model: requested_model.map(str::to_string),
            model: None,
            requested_effort: None,
            effort: None,
            provider: provider.map(str::to_string),
        })
    }

    fn no_lookup(_: Option<&str>) -> Option<String> {
        None
    }

    fn refusal_text(answer: Result<Pin, Unpinned>) -> String {
        let Err(unpinned) = answer else {
            panic!("expected a refusal")
        };
        unpinned.text
    }

    fn refusal_route(answer: Result<Pin, Unpinned>) -> Option<(String, String)> {
        let Err(unpinned) = answer else {
            panic!("expected a refusal")
        };
        unpinned.lost_route
    }

    #[test]
    fn ac1_hp_registry_model_answers_unrouted() {
        // AC1-HP: a row's requested model, no route file naming it, unrouted:
        // the candidate rides the argv with source registry.
        let answer = resolve(
            row(Some("claude-opus-5"), None),
            None,
            false,
            "11111111-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let pin = answer.unwrap();
        assert_eq!(pin.argv_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(pin.route_model, None);
        assert_eq!(pin.source, "registry");
    }

    #[test]
    fn ac1_hp_row_model_used_when_request_absent() {
        let pins = RowPins {
            requested_model: None,
            model: Some("claude-opus-5".into()),
            ..Default::default()
        };
        let answer = resolve(
            Some(pins),
            None,
            false,
            "11111111-2222-3333-4444-555555555555",
            &no_lookup,
        );
        assert_eq!(answer.unwrap().argv_model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn ac1_err_rowless_transcript_first_identity_refuses_named_route() {
        // AC1-ERR: rowless, the FIRST identity is glm-5.3-flash[1m] (the birth
        // pin) even though a later unpinned wake moved the tail to fable, and
        // a recorded route stamps zai for it: refuse naming zai and the
        // override command.
        let transcript = temp_transcript(
            "err-rowless-zai",
            &[
                attachment_line("glm-5.3-flash[1m]", None),
                assistant_line("claude-fable-5-1"),
            ],
        );
        let lookup: RouteProviderOf<'_> =
            &|m| (m == Some("glm-5.3-flash[1m]")).then(|| "zai".to_string());
        let answer = resolve(
            None,
            Some(&transcript),
            false,
            "aaaaaaaa-2222-3333-4444-555555555555",
            &lookup,
        );
        let text = refusal_text(answer);
        assert!(text.contains("route zai serves"), "{text}");
        assert!(text.contains("-P zai -m 'glm-5.3-flash[1m]'"), "{text}");
        assert!(text.contains("fno agents spawn --resume"), "{text}");
        std::fs::remove_file(&transcript).ok();
    }

    #[test]
    fn ac1_err_row_with_named_route_refuses_the_same_way() {
        // AC1-ERR: a row whose requested model a recorded route serves, but
        // whose route_settings_path is gone: the same 4a refusal.
        let lookup: RouteProviderOf<'_> =
            &|m| (m == Some("glm-5.3-flash[1m]")).then(|| "zai".to_string());
        let answer = resolve(
            row(Some("glm-5.3-flash[1m]"), Some("zai")),
            None,
            false,
            "bbbbbbbb-2222-3333-4444-555555555555",
            &lookup,
        );
        let text = refusal_text(answer);
        assert!(text.contains("-P zai -m 'glm-5.3-flash[1m]'"), "{text}");
    }

    #[test]
    fn ac1_err_route_miss_refuses_non_anthropic_row() {
        // AC1-ERR: an EMPTY route dir is a lookup miss - unknown, not
        // unrouted. A row whose provider names a non-anthropic vendor refuses
        // instead of resuming glm against the default endpoint.
        let answer = resolve(
            row(Some("glm-5.3-flash[1m]"), Some("zai")),
            None,
            false,
            "cccccccc-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let text = refusal_text(answer);
        assert!(
            text.contains("no recorded route names its provider"),
            "{text}"
        );
        assert!(
            text.contains("-P <provider> -m 'glm-5.3-flash[1m]'"),
            "{text}"
        );
    }

    #[test]
    fn ac1_err_route_miss_refuses_rowless_null_marketing_name() {
        // AC1-ERR: rowless + a lookup miss + a null marketingName on the
        // first identity: the provider cannot be established, so refuse.
        let transcript = temp_transcript(
            "err-rowless-null-mkt",
            &[attachment_line("glm-5.3-flash[1m]", None)],
        );
        let answer = resolve(
            None,
            Some(&transcript),
            false,
            "dddddddd-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let text = refusal_text(answer);
        assert!(
            text.contains("no recorded route names its provider"),
            "{text}"
        );
        std::fs::remove_file(&transcript).ok();
    }

    #[test]
    fn ac1_err_route_miss_answers_rowless_anthropic_marketing_name() {
        // AC1-ERR: the same miss beside a transcript whose identity names an
        // Anthropic model is the genuinely-unrouted claude worker: answer.
        let transcript = temp_transcript(
            "err-rowless-opus",
            &[attachment_line("claude-opus-5", Some("Opus 5"))],
        );
        let answer = resolve(
            None,
            Some(&transcript),
            false,
            "eeeeeeee-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let pin = answer.unwrap();
        assert_eq!(pin.argv_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(pin.source, "transcript");
        std::fs::remove_file(&transcript).ok();
    }

    #[test]
    fn ac1_err_route_miss_answers_anthropic_row() {
        let answer = resolve(
            row(Some("claude-opus-5"), Some("anthropic")),
            None,
            false,
            "ffff0000-2222-3333-4444-555555555555",
            &no_lookup,
        );
        assert_eq!(answer.unwrap().argv_model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn ac1_edge_transcript_fallback_answers_when_anthropic_named() {
        let transcript =
            temp_transcript("edge-named-anthropic", &[assistant_line("claude-opus-5")]);
        let lookup: RouteProviderOf<'_> =
            &|m| (m == Some("claude-opus-5")).then(|| "anthropic".to_string());
        let answer = resolve(
            None,
            Some(&transcript),
            false,
            "ffff0001-2222-3333-4444-555555555555",
            &lookup,
        );
        assert_eq!(answer.unwrap().argv_model.as_deref(), Some("claude-opus-5"));
        std::fs::remove_file(&transcript).ok();
    }

    #[test]
    fn ac1_edge_no_row_no_transcript_refuses_naming_both_reads() {
        let answer = resolve(
            None,
            None,
            false,
            "ffff0002-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let text = refusal_text(answer);
        assert!(text.contains("no registry row"), "{text}");
        assert!(text.contains("records no model"), "{text}");
        assert!(text.contains("pass --model"), "{text}");
    }

    #[test]
    fn ac1_edge_routed_resume_never_refuses_and_pins_nothing() {
        // AC1-EDGE: a routed resume with no candidate at all gets no refusal
        // and no model; the route owns the argv.
        let answer = resolve(
            None,
            None,
            true,
            "ffff0003-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let pin = answer.unwrap();
        assert_eq!(pin.argv_model, None);
        assert_eq!(pin.route_model, None);
    }

    #[test]
    fn routed_resume_records_the_candidate_as_route_model() {
        let answer = resolve(
            row(Some("claude-opus-5"), Some("anthropic")),
            None,
            true,
            "ffff0004-2222-3333-4444-555555555555",
            &no_lookup,
        );
        let pin = answer.unwrap();
        assert_eq!(pin.argv_model, None);
        assert_eq!(pin.route_model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn effort_takes_the_requested_value_first() {
        let pins = RowPins {
            requested_model: Some("claude-opus-5".into()),
            requested_effort: Some("high".into()),
            effort: Some("medium".into()),
            ..Default::default()
        };
        let answer = resolve(
            Some(pins),
            None,
            false,
            "ffff0005-2222-3333-4444-555555555555",
            &no_lookup,
        );
        assert_eq!(answer.unwrap().effort.as_deref(), Some("high"));
    }

    #[test]
    fn birth_scan_stops_at_the_first_hit() {
        // Assistant lines BEFORE the attachment do not win, and a synthetic
        // turn is skipped, not read as the running model.
        let transcript = temp_transcript(
            "birth-first-hit",
            &[
                assistant_line(SYNTHETIC_MODEL),
                attachment_line("glm-5.3-flash[1m]", None),
                assistant_line("claude-fable-5-1"),
            ],
        );
        assert_eq!(
            birth_model(&transcript).as_deref(),
            Some("glm-5.3-flash[1m]")
        );
        std::fs::remove_file(&transcript).ok();
    }

    #[test]
    fn from_entry_maps_the_recorded_axes() {
        let spawned_by = crate::state::Lineage {
            session: None,
            harness: None,
            cwd: None,
            reason: None,
        };
        let mut entry = crate::state::RegistryEntry::new(
            Some("abcdefgh-2222-3333-4444-555555555555".into()),
            spawned_by,
        );
        entry.requested_model = Some("glm-5.3-flash[1m]".into());
        entry.model = Some("glm-5.3-flash[1m]".into());
        entry.requested_effort = Some("high".into());
        entry.provider = Some("zai".into());
        let pins = RowPins::from_entry(&entry);
        assert_eq!(pins.requested_model.as_deref(), Some("glm-5.3-flash[1m]"));
        assert_eq!(pins.requested_effort.as_deref(), Some("high"));
        assert_eq!(pins.provider.as_deref(), Some("zai"));
    }

    #[test]
    fn decide_answers_the_transcript_birth_pin_over_an_empty_route_dir() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let uuid = "a1b2c3d4-9999-8888-7777-666655554444";
        let base = std::env::temp_dir().join(format!(
            "resume-pin-decide-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let proj = base.join("-Users-bb16-code-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            format!("{}\n", attachment_line("claude-opus-5", Some("Opus 5"))),
        )
        .unwrap();
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, &base);
        let route_dir = std::env::temp_dir().join(format!(
            "resume-pin-routes-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&route_dir).unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &route_dir);

        let answer = decide(&json!({"session_id": uuid, "routed": false, "row": null}));
        assert_eq!(answer["model"], json!("claude-opus-5"));
        assert_eq!(answer["source"], json!("transcript"));
        assert!(answer.get("refusal").is_none());

        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&route_dir).ok();
    }

    #[test]
    fn decide_refuses_a_rowless_glm_session_over_an_empty_route_dir() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let uuid = "a1b2c3d4-7777-6666-5555-444433332222";
        let base = std::env::temp_dir().join(format!(
            "resume-pin-decide-zai-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let proj = base.join("-Users-bb16-code-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            format!("{}\n", attachment_line("glm-5.3-flash[1m]", None)),
        )
        .unwrap();
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, &base);
        let route_dir = std::env::temp_dir().join(format!(
            "resume-pin-routes-empty-2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&route_dir).unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &route_dir);

        let answer = decide(&json!({"session_id": uuid, "routed": false, "row": null}));
        assert!(answer["refusal"]
            .as_str()
            .unwrap_or_default()
            .contains("-P <provider>"),);

        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&route_dir).ok();
    }

    #[test]
    fn ac1_hp_lookup_named_route_carries_lost_route() {
        // AC1-HP: the named-route refusal names the route that would serve
        // the birth model as (provider, model) data.
        let lookup: RouteProviderOf<'_> =
            &|m| (m == Some("glm-5.3-flash[1m]")).then(|| "zai".to_string());
        let answer = resolve(
            row(Some("glm-5.3-flash[1m]"), Some("anthropic")),
            None,
            false,
            "a1b2c3d4-0001-4000-8000-000000000001",
            &lookup,
        );
        assert!(refusal_text(answer.clone()).contains("route zai serves"));
        assert_eq!(
            refusal_route(answer),
            Some(("zai".into(), "glm-5.3-flash[1m]".into()))
        );
    }

    #[test]
    fn ac1_err_row_provider_carries_lost_route_rowless_miss_does_not() {
        // AC1-ERR: a row naming a non-anthropic provider carries the route;
        // a rowless null-marketing miss names no provider, so it does not.
        let row_answer = resolve(
            row(Some("glm-5.3-flash[1m]"), Some("zai")),
            None,
            false,
            "a1b2c3d4-0002-4000-8000-000000000002",
            &no_lookup,
        );
        assert_eq!(
            refusal_route(row_answer),
            Some(("zai".into(), "glm-5.3-flash[1m]".into()))
        );
        let rowless_answer = resolve(
            None,
            None,
            false,
            "a1b2c3d4-0003-4000-8000-000000000003",
            &no_lookup,
        );
        assert_eq!(refusal_route(rowless_answer), None);
    }

    #[test]
    fn ac1_edge_decide_emits_lost_route_object() {
        // AC1-EDGE: decide answers lost_route as {provider, model} JSON, null
        // when no provider is known. The route dir is empty here, so the row
        // arm of the lookup-miss refusal fires and names the provider.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let route_dir = std::env::temp_dir().join(format!(
            "resume-pin-routes-lost-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&route_dir).unwrap();
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &route_dir);
        let decided = decide(&json!({"session_id": "", "routed": false, "row": {
            "requested_model": "glm-5.3-flash[1m]", "provider": "zai"}}));
        assert_eq!(
            decided["lost_route"],
            json!({"provider": "zai", "model": "glm-5.3-flash[1m]"})
        );
        assert!(decided["refusal"]
            .as_str()
            .unwrap_or_default()
            .contains("-P <provider>"));

        let rowless = decide(&json!({"session_id": "", "routed": false, "row": null}));
        assert!(rowless["refusal"]
            .as_str()
            .unwrap_or_default()
            .contains("records no model"));
        assert!(rowless["lost_route"].is_null());
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&route_dir).ok();
    }

    #[test]
    fn append_axes_adds_missing_flags_only() {
        // AC1-EDGE: an explicit --model an earlier arm added always wins, and
        // a plan with neither flag gains both.
        let mut argv = vec!["claude".into(), "--resume".into(), "u".into()];
        append_axes(&mut argv, Some("claude-opus-5"), Some("high"));
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--resume".to_string(),
                "u".to_string(),
                "--model".to_string(),
                "claude-opus-5".to_string(),
                "--effort".to_string(),
                "high".to_string()
            ]
        );
        let mut pinned = vec!["claude".into(), "--model".into(), "m".into()];
        append_axes(&mut pinned, Some("claude-opus-5"), Some("high"));
        assert_eq!(pinned.iter().filter(|t| *t == "--model").count(), 1);
        assert_eq!(pinned.iter().filter(|t| *t == "--effort").count(), 1);
        let mut no_pin = vec!["claude".into()];
        append_axes(&mut no_pin, None, None);
        assert_eq!(no_pin, vec!["claude".to_string()]);
    }
}

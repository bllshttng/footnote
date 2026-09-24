//! The session phase a spawn seed names: the one verb table
//! (`spawn_phase.toml`) read through the shared verb-token parser.

use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(serde::Deserialize)]
struct Table {
    phases: HashMap<String, Vec<String>>,
}

fn verb_phases() -> &'static HashMap<String, String> {
    static PHASES: OnceLock<HashMap<String, String>> = OnceLock::new();
    PHASES.get_or_init(|| {
        let table: Table = toml::from_str(include_str!("spawn_phase.toml"))
            .expect("the bundled spawn_phase.toml parses");
        table
            .phases
            .into_iter()
            .flat_map(|(phase, verbs)| verbs.into_iter().map(move |verb| (verb, phase.clone())))
            .collect()
    })
}

/// The phase the seed's first token names. Prose, paths, unmapped verbs, and
/// empty seeds name no phase.
pub fn seed_phase(seed: &str) -> Option<&'static str> {
    let (verb, _) = crate::provider::parse_verb_token(seed.split_whitespace().next()?)?;
    verb_phases().get(verb).map(String::as_str)
}

/// A spawn request's seed is its `message` parameter.
pub fn params_seed(params: &serde_json::Value) -> Option<String> {
    params.get("message")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_phase_reads_both_sigils_and_the_table() {
        assert_eq!(seed_phase("$fno:review high --comment"), Some("review"));
        assert_eq!(seed_phase("/fno:review x"), Some("review"));
        assert_eq!(seed_phase("/code-review this diff"), Some("review"));
        assert_eq!(seed_phase("/review x-1"), Some("review"));
        assert_eq!(seed_phase("/fno:target x-1"), Some("do"));
        assert_eq!(seed_phase("  /fno:think why"), Some("think"));
    }

    #[test]
    fn prose_paths_and_unmapped_verbs_name_no_phase() {
        assert_eq!(seed_phase("review the diff"), None);
        assert_eq!(seed_phase("/Users/x/review"), None);
        assert_eq!(seed_phase("/fno:triage deep"), None);
        assert_eq!(seed_phase(""), None);
        assert_eq!(
            params_seed(&serde_json::json!({"message": "/fno:review high"})),
            Some("/fno:review high".to_string())
        );
        assert_eq!(params_seed(&serde_json::json!({})), None);
    }
}

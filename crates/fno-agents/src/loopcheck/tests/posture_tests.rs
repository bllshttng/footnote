use super::super::posture::{harness_family, peer_family, route_provider};
use super::*;

#[test]
fn resolved_optional_bots_default_matches_the_shared_python_default() {
    // One oracle, two readers: this resolver and fno.config's
    // DEFAULT_OPTIONAL_APPS both answer these rows, so a drift on either
    // side fails its own test against the SAME file.
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../cli/tests/config/optional_apps_default.json");
    let text = std::fs::read_to_string(&golden)
        .unwrap_or_else(|e| panic!("read golden {}: {e}", golden.display()));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let unset = v["unset"].as_array().unwrap();
    let unset: Vec<String> = unset
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    // Unset (None) -> the built-in default.
    let settings = Settings::default();
    assert_eq!(resolved_optional_bots(&settings), unset);
    // An explicit [] is a real opt-out and wins over the default.
    let empty = Settings {
        optional_apps: Some(Vec::new()),
        ..Settings::default()
    };
    assert!(resolved_optional_bots(&empty).is_empty());
    // A partial list EXTENDS the built-ins (round-2 ruling): it never
    // silently drops a honored-if-present login.
    let partial = v["partial_union"].as_array().unwrap();
    let partial: Vec<String> = partial
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    let mine = Settings {
        optional_apps: Some(vec!["my-app".to_string()]),
        ..Settings::default()
    };
    assert_eq!(resolved_optional_bots(&mine), partial);
}

#[test]
fn is_bot_reviewer_known_patterns() {
    assert!(is_bot_reviewer("gemini-code-assist[bot]", &[]));
    assert!(is_bot_reviewer("chatgpt-codex-connector", &[]));
    assert!(is_bot_reviewer("some-bot[bot]", &[]));
    assert!(!is_bot_reviewer("human-reviewer", &[]));
}

#[test]
fn is_bot_reviewer_with_external_list() {
    let external = vec!["my-bot".to_string()];
    // "my-bot" is a substring of "my-bot" -> match via configured list
    assert!(is_bot_reviewer("my-bot", &external));
    // "other-bot[bot]" doesn't match "my-bot" substring, but falls back to
    // the [bot] suffix heuristic (configured list must not make reviewed unreachable)
    assert!(is_bot_reviewer("other-bot[bot]", &external));
}

#[test]
fn is_bot_reviewer_configured_short_names_match_real_logins() {
    // Fix 1: configured entries use substring matching.
    // "gemini" (short config name) must match "gemini-code-assist[bot]"
    // "codex" must match "chatgpt-codex-connector"
    let external = vec!["gemini".to_string(), "codex".to_string()];
    assert!(
        is_bot_reviewer("gemini-code-assist[bot]", &external),
        "gemini short name must substring-match gemini-code-assist[bot]"
    );
    assert!(
        is_bot_reviewer("chatgpt-codex-connector", &external),
        "codex short name must substring-match chatgpt-codex-connector"
    );
}

#[test]
fn is_bot_reviewer_configured_list_falls_back_to_bot_heuristic() {
    // Fix 1: when configured list has [some-human] but a bot review arrives,
    // fallback to endswith-[bot] heuristic so reviewed remains reachable.
    let external = vec!["some-human".to_string()];
    assert!(
        is_bot_reviewer("gemini-code-assist[bot]", &external),
        "configured list with no match must still fall back to [bot] heuristic"
    );
}

#[test]
fn is_bot_reviewer_empty_config_human_only_returns_false() {
    // Fix 1: empty config + human-only review -> false
    assert!(
        !is_bot_reviewer("alice-the-human", &[]),
        "human reviewer with empty config must return false"
    );
}

#[test]
fn resolved_required_bots_default_is_empty() {
    // Fresh-install default: no required review bot, so a clone with no
    // review configuration is not blocked waiting for a bot it never set up.
    let s = Settings::default();
    assert!(
        resolved_required_bots(&s).is_empty(),
        "absent required_bots config must resolve to no review gate"
    );
}

#[test]
fn resolved_required_bots_explicit_list_wins() {
    let s = Settings {
        required_bots: Some(vec!["my-bot".to_string()]),
        ..Default::default()
    };
    assert_eq!(resolved_required_bots(&s), vec!["my-bot".to_string()]);
    let empty = Settings {
        required_bots: Some(Vec::new()),
        ..Default::default()
    };
    assert!(resolved_required_bots(&empty).is_empty());
}

#[test]
fn resolved_optional_is_separate_from_required() {
    // An optional-only config leaves the REQUIRED set empty (never waited
    // on) while the optional set carries the honored-if-present login.
    let s = parse_settings(
        "[review]\ngithub_apps = []\noptional_apps = [\"chatgpt-codex-connector\"]\n",
    );
    assert!(
        resolved_required_bots(&s).is_empty(),
        "optional must not be required"
    );
    // A non-empty configured list EXTENDS the built-ins, so this config
    // (built-in named explicitly) resolves to both built-ins.
    assert_eq!(
        resolved_optional_bots(&s),
        vec![
            "gemini-code-assist".to_string(),
            "chatgpt-codex-connector".to_string()
        ]
    );
}

#[test]
fn resolved_github_apps_wins_over_required_bots_alias() {
    // Both set -> github_apps wins (Locked Decision 2).
    let s = Settings {
        github_apps: Some(vec!["new-bot".to_string()]),
        required_bots: Some(vec!["old-bot".to_string()]),
        ..Default::default()
    };
    assert_eq!(resolved_required_bots(&s), vec!["new-bot".to_string()]);
    // required_bots-only still gates (legacy alias, AC2-HP).
    let legacy = Settings {
        required_bots: Some(vec!["old-bot".to_string()]),
        ..Default::default()
    };
    assert_eq!(resolved_required_bots(&legacy), vec!["old-bot".to_string()]);
}

#[test]
fn resolved_peers_shared_identity_collapses_to_one_login() {
    // Scalar peers share peer_identity -> the gate is that one login on top
    // of github_apps (AC1-HP: no App bot, just the peer identity).
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![
            PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            },
            PeerEntry {
                provider: "gemini".into(),
                model: None,
                identity: None,
            },
        ],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    assert_eq!(resolved_required_bots(&s), vec!["fno-peer-bot".to_string()]);
}

#[test]
fn resolved_peers_per_entry_identities_each_add_a_login() {
    let s = Settings {
        github_apps: Some(vec!["chatgpt-codex-connector".into()]),
        peers: vec![
            PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: Some("fno-codex-bot".into()),
            },
            PeerEntry {
                provider: "gemini".into(),
                model: None,
                identity: Some("fno-gemini-bot".into()),
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        resolved_required_bots(&s),
        vec![
            "chatgpt-codex-connector".to_string(),
            "fno-codex-bot".to_string(),
            "fno-gemini-bot".to_string(),
        ]
    );
}

#[test]
fn identity_free_peer_uses_local_attestation_not_a_login() {
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![PeerEntry {
            provider: "gemini".into(),
            model: None,
            identity: None,
        }],
        peer_identity: None,
        ..Default::default()
    };
    assert!(resolved_required_bots_for_author(&s, Some("codex")).is_empty());
    assert_eq!(
        resolved_local_peer_reviewers_for_author(&s, Some("codex")),
        vec![LOCAL_PEER_REVIEWER.to_string()]
    );
}

#[test]
fn identity_free_same_model_peer_is_an_unsatisfiable_local_gate() {
    let s = Settings {
        peers: vec![PeerEntry {
            provider: "codex".into(),
            model: None,
            identity: None,
        }],
        ..Default::default()
    };
    assert_eq!(
        resolved_local_peer_reviewers_for_author(&s, Some("codex")),
        vec![SAME_MODEL_LOCAL_PEER_SENTINEL.to_string()]
    );
}

#[test]
fn identity_free_mixed_peers_form_one_composite_gate() {
    let s = Settings {
        peers: vec![
            PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            },
            PeerEntry {
                provider: "claude".into(),
                model: Some("zai,glm-5.2".into()),
                identity: None,
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        resolved_local_peer_reviewers_for_author(&s, Some("codex")),
        vec![LOCAL_PEER_REVIEWER.to_string()]
    );
}

#[test]
fn explicit_peer_identity_keeps_login_gate_only() {
    let s = Settings {
        peers: vec![PeerEntry {
            provider: "gemini".into(),
            model: None,
            identity: Some("fno-gemini-bot".into()),
        }],
        ..Default::default()
    };
    assert_eq!(
        resolved_required_bots_for_author(&s, Some("codex")),
        vec!["fno-gemini-bot".to_string()]
    );
    assert!(resolved_local_peer_reviewers_for_author(&s, Some("codex")).is_empty());
}

/// US5: effective model family resolution across bare providers, routes,
/// malformed routes (fall back to provider), and unknown providers (None).
#[test]
fn peer_family_mapping_table() {
    let bare = |p: &str| PeerEntry {
        provider: p.into(),
        model: None,
        identity: None,
    };
    let routed = |p: &str, m: &str| PeerEntry {
        provider: p.into(),
        model: Some(m.into()),
        identity: None,
    };
    // harness_family: names + aliases + case-insensitivity; unknown -> None.
    assert_eq!(harness_family("claude"), Some("anthropic"));
    assert_eq!(harness_family("ANTHROPIC"), Some("anthropic"));
    assert_eq!(harness_family("codex"), Some("openai"));
    assert_eq!(harness_family("gemini"), Some("google"));
    assert_eq!(harness_family("zai"), None);
    // route_provider: exactly two non-empty parts, else None (fall back).
    assert_eq!(route_provider("zai,glm-5.2"), Some("zai"));
    assert_eq!(route_provider(" openai , gpt-5 "), Some("openai"));
    assert_eq!(route_provider("gpt-5"), None); // no comma -> malformed
    assert_eq!(route_provider("zai,"), None); // empty model -> malformed
    assert_eq!(route_provider(",glm"), None); // empty provider -> malformed
    assert_eq!(route_provider("a,b,c"), None); // three parts -> malformed

    // peer_family: bare provider, valid route wins, malformed falls back.
    assert_eq!(peer_family(&bare("codex")), Some("openai"));
    assert_eq!(peer_family(&bare("grok")), None); // unknown -> never matches
    assert_eq!(peer_family(&routed("claude", "zai,glm-5.2")), None); // route wins
    assert_eq!(
        peer_family(&routed("codex", "openai,gpt-5")),
        Some("openai")
    );
    assert_eq!(peer_family(&routed("codex", "gpt-5")), Some("openai")); // malformed -> provider
}

/// AC1-HP: codex author + `peers: [codex]` -> the peer login is replaced by
/// the same-model sentinel so the gate cannot clear.
#[test]
fn same_model_peer_holds_gate() {
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![PeerEntry {
            provider: "codex".into(),
            model: None,
            identity: None,
        }],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("codex"));
    assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL));
    assert!(!logins.iter().any(|l| l == "fno-peer-bot"));
}

/// AC2-HP: codex author + `peers: [gemini]` (cross-model) clears exactly as
/// today - the login stays, no sentinel.
#[test]
fn cross_model_peer_login_unchanged() {
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![PeerEntry {
            provider: "gemini".into(),
            model: None,
            identity: None,
        }],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("codex"));
    assert_eq!(logins, vec!["fno-peer-bot".to_string()]);
}

/// US1 / step-3b: a claude author with a routed claude peer
/// (`{provider: claude, model: "zai,glm-5.2"}`) is cross-model (GLM via zai)
/// -> the login stays.
#[test]
fn routed_claude_peer_is_cross_model() {
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![PeerEntry {
            provider: "claude".into(),
            model: Some("zai,glm-5.2".into()),
            identity: None,
        }],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("claude"));
    assert_eq!(logins, vec!["fno-peer-bot".to_string()]);
}

/// AC3-ERR: a claude peer routed back to the author's own family
/// (`anthropic,...`, hand-edited past the loader) is same-model -> sentinel.
#[test]
fn same_family_route_holds_gate() {
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![PeerEntry {
            provider: "claude".into(),
            model: Some("anthropic,claude-opus".into()),
            identity: None,
        }],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("claude"));
    assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL));
    assert!(!logins.iter().any(|l| l == "fno-peer-bot"));
}

/// AC5-EDGE: codex author + `peers: [codex, gemini]` sharing one identity
/// stays satisfiable (gemini backs the login) -> login kept, no sentinel.
#[test]
fn shared_identity_mixed_peers_stays_satisfiable() {
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![
            PeerEntry {
                provider: "codex".into(),
                model: None,
                identity: None,
            },
            PeerEntry {
                provider: "gemini".into(),
                model: None,
                identity: None,
            },
        ],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("codex"));
    assert_eq!(logins, vec!["fno-peer-bot".to_string()]);
}

/// AC6-FR: unknown harness (None) leaves the login set byte-identical to the
/// no-guard wrapper, even for a would-be same-model config.
#[test]
fn unknown_harness_is_byte_identical() {
    let s = Settings {
        github_apps: Some(vec!["chatgpt-codex-connector".into()]),
        peers: vec![PeerEntry {
            provider: "codex".into(),
            model: None,
            identity: None,
        }],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    // None author => guard inert => equals the no-harness wrapper exactly.
    assert_eq!(
        resolved_required_bots_for_author(&s, None),
        resolved_required_bots(&s)
    );
    assert!(!resolved_required_bots_for_author(&s, None)
        .iter()
        .any(|l| l == SAME_MODEL_PEER_SENTINEL));
}

/// A same-model peer whose identity COLLIDES with a required App login is
/// fail-closed, not exempt (codex peer review on PR #375): the App login is
/// kept (its requirement is not loosened) AND the sentinel is added, so a
/// same-model review under the shared login cannot clear the gate.
#[test]
fn base_app_login_collision_is_fail_closed() {
    let s = Settings {
        github_apps: Some(vec!["fno-peer-bot".into()]),
        peers: vec![PeerEntry {
            provider: "codex".into(),
            model: None,
            identity: None,
        }],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("codex"));
    assert!(logins.iter().any(|l| l == "fno-peer-bot")); // App requirement kept
    assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL)); // gate held
}

/// A codex/gemini peer's `model` route is NOT honored (only claude transport
/// executes a route; codex/gemini dispatch runs the bare provider). A codex
/// peer with a zai route stays openai-family -> same-model on a codex author,
/// closing the route-bypass codex flagged on PR #375.
#[test]
fn non_claude_route_is_ignored() {
    let routed_codex = PeerEntry {
        provider: "codex".into(),
        model: Some("zai,glm-5.2".into()),
        identity: None,
    };
    assert_eq!(peer_family(&routed_codex), Some("openai"));
    let s = Settings {
        github_apps: Some(Vec::new()),
        peers: vec![routed_codex],
        peer_identity: Some("fno-peer-bot".into()),
        ..Default::default()
    };
    let logins = resolved_required_bots_for_author(&s, Some("codex"));
    assert!(logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL));
    assert!(!logins.iter().any(|l| l == "fno-peer-bot"));
}

#[test]
fn login_matches_bot_cases() {
    // Full login, [bot]-suffixed login, and short config names all match.
    assert!(login_matches_bot(
        "chatgpt-codex-connector",
        "chatgpt-codex-connector"
    ));
    assert!(login_matches_bot(
        "chatgpt-codex-connector[bot]",
        "chatgpt-codex-connector"
    ));
    assert!(login_matches_bot("chatgpt-codex-connector", "codex"));
    assert!(login_matches_bot("Gemini-Code-Assist[bot]", "gemini"));
    assert!(!login_matches_bot("alice-the-human", "codex"));
    // Empty config entry must never match every login.
    assert!(!login_matches_bot("anyone", ""));
}

//! Which reviewers does this change owe? The posture ladder, required bots, peer reviewers, and the same-model guard.

use super::*;

/// Default must-have-reviewed list when config.review.github_apps is absent.
/// EMPTY for fresh installs: a clone with no review configuration completes on
/// PR + CI green without hanging on a review bot it has never set up (a fresh
/// `/target` otherwise runs to the budget cap waiting for a codex review that
/// never arrives). Maintainers who want an external-review gate pin it
/// explicitly via config.review.github_apps (e.g. ["chatgpt-codex-connector"]).
pub(super) const DEFAULT_REQUIRED_BOTS: &[&str] = &[];

/// Stable reviewer key emitted by every identity-free peer. Multiple configured
/// peer harnesses are alternatives for one composite gate, not N required votes.
// ── review.posture  ──────────────────────────────────────────────────
//
// The nine-rung ladder, mirrored from `fno.config.REVIEW_POSTURES`. One leaf
// names how much review a code PR must have; coverage resolves the rung and
// reports satisfaction against the verdicts it already computed. Python reads
// the emitted fields and never reclassifies them (the Ownership rule).

/// Component vocabulary -> (rank, cost, freshness, diversity) per rung. The
/// component strings are the shared vocabulary: `self`, `independent`,
/// `github`, `peer`. `check-reviewer-descriptor-parity.sh` pins ranks and
/// costs against the Python table.
pub(super) fn posture_rung(
    value: &str,
) -> Option<(
    &'static [&'static str],
    i64,
    &'static str,
    &'static str,
    &'static str,
)> {
    match value {
        "no_review" => Some((&[], 1, "zero reviews", "none", "none")),
        "tests_pass" => Some((&[], 2, "zero reviews", "none", "none")),
        "self_review" => Some((
            &["self"],
            3,
            "one review",
            "same context sufficient",
            "same model allowed",
        )),
        "independent_review" => Some((
            &["independent"],
            4,
            "one fresh reviewer",
            "fresh reviewer context required",
            "same model allowed",
        )),
        "github_review" => Some((
            &["github"],
            5,
            "one App review",
            "external",
            "configured App",
        )),
        "peer_review" => Some((
            &["peer"],
            6,
            "one peer review",
            "fresh reviewer context required",
            "different model family required",
        )),
        "self_and_github" => Some((&["self", "github"], 7, "two reviews", "mixed", "mixed")),
        "self_and_peer" => Some((&["self", "peer"], 8, "two reviews", "mixed", "mixed")),
        "self_github_and_peer" => Some((
            &["self", "github", "peer"],
            9,
            "three reviews",
            "mixed",
            "mixed",
        )),
        _ => None,
    }
}

/// Lookup used by the settings parser: value -> components only.
pub(super) fn posture_components(value: &str) -> Option<&'static [&'static str]> {
    posture_rung(value).map(|(c, _, _, _, _)| c)
}

/// The resolved posture carried into read_pr_info. Computed once by the
/// caller (which owns the parsed settings), evaluated against verdicts there.
#[derive(Debug, Clone, Serialize)]
pub struct PostureConfig {
    pub value: String,
    pub rank: i64,
    pub components: &'static [&'static str],
    /// explicit | legacy | default - how the rung was resolved.
    pub source: &'static str,
    pub cost: &'static str,
    pub freshness: &'static str,
    pub diversity: &'static str,
}

/// The authoritative posture verdict serialized on every `review_coverage`
/// row that carries a resolved posture. Python reads these fields verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct PostureVerdict {
    pub posture: String,
    pub rank: i64,
    pub source: String,
    pub cost: String,
    pub freshness: String,
    pub diversity: String,
    pub posture_satisfied: bool,
    pub posture_gaps: Vec<String>,
}

/// Resolve the one posture a settings block names, mirroring
/// `fno.config.resolve_review_posture` signal for signal. Explicit wins;
/// absent infers from the legacy settings (preserving an explicit
/// `self_review_required=false` opt-out and a declared-empty gate); a bare
/// install is the shipped DEFAULT floor (rung 3), never an inference.
///
/// One deliberate divergence, inherited from the gate itself: Rust honors an
/// explicit `self_review_required=false` only behind a LIVE opt-out claim, so
/// `floor_off` reads the EFFECTIVE value. A config carrying an unclaimed
/// false reads floor-on here, which errs toward holding the gate - never
/// toward clearing it.
/// The resolved `review.carry_interdiff_lines` (law d-608344c1): the law's
/// default is 100; `0` disables the arm; a negative config value is a typo and
/// reads as the default rather than as a refusal nobody asked for.
pub(super) fn carry_interdiff_lines_resolved(settings: &Settings) -> usize {
    match settings.carry_interdiff_lines {
        Some(n) if n >= 0 => n as usize,
        _ => 100,
    }
}

pub(super) fn resolve_posture_config(settings: &Settings) -> PostureConfig {
    if let Some(p) = settings.posture.as_deref() {
        if let Some((components, rank, cost, freshness, diversity)) = posture_rung(p) {
            return PostureConfig {
                value: p.to_string(),
                rank,
                components,
                source: "explicit",
                cost,
                freshness,
                diversity,
            };
        }
    }
    // Legacy inference. github_apps wins over the required_bots alias when
    // both are set, the same way resolved_required_bots_for_author resolves.
    let resolved_gate = settings
        .github_apps
        .as_ref()
        .or(settings.required_bots.as_ref());
    let github = matches!(resolved_gate, Some(l) if !l.is_empty());
    let declared_none = matches!(resolved_gate, Some(l) if l.is_empty());
    let peers = !settings.peers.is_empty();
    let floor_off = settings.self_review_required == Some(false);
    let self_named = settings.reviewers.iter().any(|r| {
        // strip() then lstrip('/'), the exact two passes Python's inference
        // applies: a padded or slash-prefixed excluded name must read excluded
        // on both sides, or the two resolvers disagree on the rung.
        let n = r.trim().trim_start_matches('/');
        n != "declare" && n != "sigma"
    });
    let any_signal = github || declared_none || peers || floor_off || self_named;
    let value = if declared_none && !github && !peers {
        "tests_pass"
    } else if github && peers {
        if floor_off {
            "github_review"
        } else {
            "self_github_and_peer"
        }
    } else if github {
        if floor_off {
            "github_review"
        } else {
            "self_and_github"
        }
    } else if peers {
        if floor_off {
            "peer_review"
        } else {
            "self_and_peer"
        }
    } else if self_named {
        "self_review"
    } else if floor_off {
        // An explicit (claim-backed) opt-out with no other lane.
        "no_review"
    } else {
        "self_review"
    };
    let source = if any_signal { "legacy" } else { "default" };
    let (components, rank, cost, freshness, diversity) = posture_rung(value).expect("ladder value");
    PostureConfig {
        value: value.to_string(),
        rank,
        components,
        source,
        cost,
        freshness,
        diversity,
    }
}

/// Evaluate the resolved posture against the coverage verdicts. Every
/// unsatisfied component names its exact gap; `declare` and `sigma` never
/// satisfy the self lane, any other Reviewed verdict does whatever produced
/// it, and the peer lane counts only verdicts the cross-model resolver
/// admits (the same-model sentinel never matches a real reviewer name).
pub(super) fn posture_verdict(
    config: &PostureConfig,
    rep: &CoverageReport,
    peer_reviewers: &[String],
) -> PostureVerdict {
    let satisfies = |component: &str| match component {
        "self" => rep.verdicts.iter().any(|v| {
            v.verdict == CoverageVerdict::Reviewed && v.name != "declare" && v.name != "sigma"
        }),
        "independent" => rep.verdicts.iter().any(|v| {
            v.producer == CoverageProducer::LocalAttestation
                && v.verdict == CoverageVerdict::Reviewed
                && v.name != "declare"
                && v.name != "sigma"
                && v.reviewer_context.as_deref() == Some("fresh")
        }),
        "github" => rep.verdicts.iter().any(|v| {
            v.producer == CoverageProducer::GithubApp && v.verdict == CoverageVerdict::Reviewed
        }),
        "peer" => rep.verdicts.iter().any(|v| {
            v.producer == CoverageProducer::LocalAttestation
                && v.verdict == CoverageVerdict::Reviewed
                && peer_reviewers.iter().any(|p| p == &v.name)
        }),
        _ => false,
    };
    let gaps: Vec<String> = config
        .components
        .iter()
        .filter(|c| !satisfies(*c))
        .map(|c| match *c {
            "self" => {
                "self: no real final-head review at this head (declare/sigma excluded)".to_string()
            }
            "independent" => {
                "independent: no review with positive fresh-context provenance (reviewer_context=fresh) at this head".to_string()
            }
            "github" => "github: no configured GitHub App review at this head".to_string(),
            "peer" => "peer: no cross-model peer verdict at this head".to_string(),
            other => format!("{other}: unsatisfied"),
        })
        .collect();
    PostureVerdict {
        posture: config.value.clone(),
        rank: config.rank,
        source: config.source.to_string(),
        cost: config.cost.to_string(),
        freshness: config.freshness.to_string(),
        diversity: config.diversity.to_string(),
        posture_satisfied: gaps.is_empty(),
        posture_gaps: gaps,
    }
}

pub(super) const LOCAL_PEER_REVIEWER: &str = "peer";

/// An unmatchable reviewer key used when every identity-free peer is the
/// author's own model family. It keeps the local gate fail-closed independently
/// of the producer and is rendered as an actionable same-model refusal.
pub(super) const SAME_MODEL_LOCAL_PEER_SENTINEL: &str = "\u{0}fno-peer-same-model-local\u{0}";

/// A login no real GitHub account can equal, pushed when a required peer login is
/// backed ONLY by peers whose model is the author's own (same-model guard). It
/// REPLACES the clearable login so a same-model review can never satisfy the
/// cross-model gate.
pub(super) const SAME_MODEL_PEER_SENTINEL: &str = "\u{0}fno-peer-same-model\u{0}";

/// Model family of a harness or provider name - the same-model guard's proxy for
/// "which model". The author's family is its invoking harness's family
/// (claude->anthropic, codex->openai, gemini->google); a peer's family is its
/// route provider (else its bare provider). An unknown name is None and so never
/// equals any author family (fail open per-peer). A routed-transport author
/// (claude CLI over GLM) still reads as anthropic here - a known limitation that
/// errs toward HOLDING the gate, never wrongly clearing it.
pub(super) fn harness_family(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some("anthropic"),
        "codex" | "openai" => Some("openai"),
        "gemini" | "google" => Some("google"),
        _ => None,
    }
}

/// The route provider of a peers `model` route: `"route_provider,route_model"`
/// -> `route_provider`. None unless there are exactly two non-empty comma parts,
/// matching the loader's parse rule (config/__init__.py coerce_peers), so a
/// malformed route falls back to the bare provider.
pub(super) fn route_provider(model: &str) -> Option<&str> {
    let mut parts = model.split(',').map(str::trim);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(prov), Some(rest), None) if !prov.is_empty() && !rest.is_empty() => Some(prov),
        _ => None,
    }
}

/// A peer's effective model family: its route provider's family when it names a
/// valid route, else its bare provider's family. A `model` route is only honored
/// for a **claude** peer, because only the claude transport actually executes a
/// route (`claude -p` over the routed model); codex/gemini dispatch ignores the
/// route and runs the bare provider, so trusting a codex/gemini route would
/// classify a same-model review as cross-model and re-open the bypass this guard
/// exists to close. Matches the loader, which validates routes for claude only.
pub(super) fn peer_family(peer: &PeerEntry) -> Option<&'static str> {
    let effective = peer
        .model
        .as_deref()
        .filter(|_| peer.provider.trim().eq_ignore_ascii_case("claude"))
        .and_then(route_provider)
        .unwrap_or(peer.provider.as_str());
    harness_family(effective)
}

/// Thin wrapper: resolve the must-have-reviewed login set with NO author-harness
/// awareness (the same-model guard is inert). Test-only convenience so existing
/// tests stay byte-identical; production passes the resolved harness via
/// [`resolved_required_bots_for_author`].
#[cfg(test)]
pub(super) fn resolved_required_bots(settings: &Settings) -> Vec<String> {
    resolved_required_bots_for_author(settings, None)
}

/// The set of expected review logins that must have passed for the gate to
/// clear: `github_apps` (or its legacy `required_bots` alias) UNION
/// the resolved posting identity of each identity-backed `peers` entry.
/// Identity-free peers are resolved separately into local reviewer evidence.
///
/// `author_harness` is the invoking harness (`claude`/`codex`/`gemini`), resolved
/// from the ambient env markers by the caller. When it resolves to a model
/// family, the same-model guard replaces any peer login backed ONLY by
/// the author's own model with SAME_MODEL_PEER_SENTINEL, so a codex-authored run
/// with `peers: [codex]` can no longer review its own work and clear the gate.
/// `None` (unknown authorship) leaves the login set byte-identical - fail open.
pub(super) fn resolved_required_bots_for_author(
    settings: &Settings,
    author_harness: Option<&str>,
) -> Vec<String> {
    // github_apps wins over the legacy required_bots alias when both are set.
    if settings.github_apps.is_some() && settings.required_bots.is_some() {
        eprintln!(
            "loop-check: both config.review.github_apps and required_bots set - using github_apps"
        );
    }
    let mut logins: Vec<String> = match settings
        .github_apps
        .as_ref()
        .or(settings.required_bots.as_ref())
    {
        Some(list) => list.clone(),
        None => DEFAULT_REQUIRED_BOTS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };

    // Only identity-backed peers contribute to the expected-login set. Shared
    // identity collapses to one login; per-peer identities each add their own.
    // Identity-free peers are not missing logins: they use local attestations.
    for peer in &settings.peers {
        let id = peer
            .identity
            .clone()
            .or_else(|| settings.peer_identity.clone());
        match id {
            Some(id) if !logins.iter().any(|l| l == &id) => logins.push(id),
            Some(_) => {} // already present (shared identity)
            None => {}    // local-attestation carrier
        }
    }

    // Same-model guard: a peer login backed ONLY by the author's own
    // model cannot honestly satisfy the cross-model gate. Inert unless the
    // author harness resolves to a family (fail open on unknown authorship, so
    // the block above stays byte-identical). The GITHUB_APPS base set is never
    // touched - only logins contributed by `peers` are eligible.
    if let Some(author) = author_harness.filter(|_| !settings.peers.is_empty()) {
        if let Some(author_fam) = harness_family(author) {
            apply_same_model_guard(&mut logins, settings, author, author_fam);
        }
    }
    logins
}

/// Resolve all identity-free peers into one local reviewer requirement.
///
/// Any cross-model option makes the composite gate satisfiable by a `peer`
/// attestation. When the author is known and every option is same-model, return
/// an unmatchable sentinel so even a forged `peer: pass` cannot self-review the
/// change. Unknown peer families remain eligible, matching the existing
/// identity-backed guard's conservative compatibility rule.
pub(super) fn resolved_local_peer_reviewers_for_author(
    settings: &Settings,
    author_harness: Option<&str>,
) -> Vec<String> {
    if settings.peer_identity.is_some() {
        return Vec::new();
    }
    let local: Vec<&PeerEntry> = settings
        .peers
        .iter()
        .filter(|peer| peer.identity.is_none())
        .collect();
    if local.is_empty() {
        return Vec::new();
    }
    let Some(author_fam) = author_harness.and_then(harness_family) else {
        return vec![LOCAL_PEER_REVIEWER.to_string()];
    };
    if local
        .iter()
        .any(|peer| peer_family(peer) != Some(author_fam))
    {
        vec![LOCAL_PEER_REVIEWER.to_string()]
    } else {
        eprintln!(
            "loop-check: every identity-free peer is the author's own model - configure a cross-model peer or routed model"
        );
        vec![SAME_MODEL_LOCAL_PEER_SENTINEL.to_string()]
    }
}

/// Replace every peer-contributed login backed ONLY by same-model peers with
/// SAME_MODEL_PEER_SENTINEL and print one loud line per such login. A login with
/// >=1 cross-model peer (a different family, or an unknown provider) is left
/// alone. When a same-model peer login COLLIDES with a github_apps/required_bots
/// base login (`peer_identity` == an App login), the base login is kept (its App
/// requirement is not loosened) AND the sentinel is appended, so a same-model
/// review posted under that shared login can never be the thing that clears the
/// gate - the collision is a fail-closed hold, not an exemption (codex peer
/// review on PR #375). Peers are walked in config order so output is deterministic.
pub(super) fn apply_same_model_guard(
    logins: &mut Vec<String>,
    settings: &Settings,
    author_harness: &str,
    author_fam: &str,
) {
    let base_set = settings
        .github_apps
        .as_ref()
        .or(settings.required_bots.as_ref());

    // Per distinct peer login, in first-seen order: does any backing peer differ
    // in model family, and the first same-model provider (for the message)?
    let mut seen: Vec<(String, bool, String)> = Vec::new();
    for peer in &settings.peers {
        let Some(login) = peer
            .identity
            .as_deref()
            .or(settings.peer_identity.as_deref())
        else {
            continue;
        };
        let cross = peer_family(peer) != Some(author_fam);
        match seen.iter_mut().find(|(l, _, _)| l.as_str() == login) {
            Some(entry) => entry.1 = entry.1 || cross,
            None => seen.push((login.to_string(), cross, peer.provider.clone())),
        }
    }

    for (login, any_cross, provider) in seen {
        if any_cross {
            continue;
        }
        if base_set.is_some_and(|set| set.contains(&login)) {
            // Collision: the peer posts under a required App login. Keep the App
            // requirement, but add the sentinel so this same-model login can't be
            // what clears the gate (never an exemption - fail closed).
            if !logins.iter().any(|l| l == SAME_MODEL_PEER_SENTINEL) {
                logins.push(SAME_MODEL_PEER_SENTINEL.to_string());
            }
        } else if let Some(slot) = logins.iter_mut().find(|l| **l == login) {
            // Peer-only login: replace it with the sentinel.
            *slot = SAME_MODEL_PEER_SENTINEL.to_string();
        }
        eprintln!(
            "loop-check: peer '{provider}' is the author's own model ({author_harness}-authored run) - the cross-model gate cannot be satisfied by it; configure a cross-model peer or a model route"
        );
    }
}

/// The built-in optional logins an UNSET `review.optional_apps` resolves to -
/// the same default the Python side resolves (`DEFAULT_OPTIONAL_APPS` in
/// fno.config, pinned with it against the shared golden file
/// cli/tests/config/optional_apps_default.json). Without a shared default, this
/// side resolved empty while `fno do pr status` matched two hardcoded logins,
/// so a worker following the remedy one printed was refused by the other.
pub(super) const DEFAULT_OPTIONAL_APPS: [&str; 2] =
    ["gemini-code-assist", "chatgpt-codex-connector"];

/// The OPTIONAL reviewer logins (config.review.optional_apps): honored-if-
/// present but never required. Their blocking findings hold the gate, but their
/// absence never does. Unset resolves to the
/// built-in default; an explicit `[]` is a real opt-out and wins over it.
pub(super) fn resolved_optional_bots(settings: &Settings) -> Vec<String> {
    match settings.optional_apps.clone() {
        // Unset: the built-in honored-if-present logins.
        None => DEFAULT_OPTIONAL_APPS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // Explicit [] is the real opt-out (the one behavior this default
        // changes on purpose, documented in the registry Meta).
        Some(configured) if configured.is_empty() => Vec::new(),
        // A non-empty list EXTENDS the built-ins: it named extra honored
        // logins since before the default existed, and silently dropping a
        // built-in from a partial list would stop counting that login's
        // blocking findings on configs that never asked for it.
        Some(configured) => {
            let mut all = DEFAULT_OPTIONAL_APPS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            for login in configured {
                if !all.contains(&login) {
                    all.push(login);
                }
            }
            all
        }
    }
}

/// Case-insensitive substring match so a configured short name ("codex") or a
/// full login both match the review author, including gh's `[bot]`-suffixed
/// form (reference_gh_bot_login_suffix_polling_trap).
pub(crate) fn login_matches_bot(login: &str, bot: &str) -> bool {
    !bot.is_empty() && login.to_lowercase().contains(&bot.to_lowercase())
}

/// Exact-login equality, case-insensitive the way GitHub treats logins.
/// Deliberately NOT `login_matches_bot`'s substring match: "ali" must not
/// read as the author "alice" when the approval-counting rule asks whether
/// the approver IS the author.
pub(super) fn login_equals(a: &str, b: &str) -> bool {
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

pub(super) fn is_bot_reviewer(login: &str, external_reviewers: &[String]) -> bool {
    if !external_reviewers.is_empty() {
        let login_lower = login.to_lowercase();
        // Case-insensitive substring match: "gemini" matches "gemini-code-assist[bot]"
        if external_reviewers
            .iter()
            .any(|r| login_lower.contains(&r.to_lowercase()))
        {
            return true;
        }
        // Configured list present but no entry matched: fall back to bot heuristic
        // so a configured-but-partial list doesn't make reviewed unreachable.
    }
    // Default: endswith [bot] or a known profile login
    login.ends_with("[bot]") || BOT_PROFILES.iter().any(|p| login.contains(p.login))
}

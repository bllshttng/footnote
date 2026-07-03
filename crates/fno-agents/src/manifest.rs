//! Detection manifest engine (E6.2).
//!
//! E6.1 ([`crate::osc`], [`crate::screen`]) gave the read loop OSC title/progress
//! as detection regions on a [`ScreenView`]. This module is the engine that turns
//! a declarative TOML rule file into a state verdict over that view, so an agent's
//! readiness rules live in a `*.toml` (authored in E6.3) instead of hardcoded Rust
//! ([`crate::readiness`]).
//!
//! A manifest is a priority-ordered list of [`ManifestRule`]s. Each rule names a
//! text [`Region`] of the screen and a recursive boolean [`Gate`] over that
//! region's text. [`Manifest::evaluate`] returns the highest-priority matching
//! rule's `state` (and its `skip_state_update` flag) - so a "yes" buried in
//! scrollback never out-votes a live-region rule that out-prioritizes it.
//!
//! Scope: E6.2 built the parser + region vocabulary + gate evaluator + priority
//! arbiter; E6.3 added the bundled `claude.toml`/`codex.toml`/`gemini.toml` rule
//! files and the [`load_manifest`] resolution chain (bundled + local override).
//! Still NOT wired into the runtime: the daemon state badge consumes
//! [`Manifest::evaluate`] only once E2 lands live claude panes to tune against.
//! Remote/cached/version-gated resolution is a logged fast-follow
//! (`min_engine_version` is parsed now so a later remote manifest can gate, but
//! is otherwise unused).
//!
//! ponytail: a rule's regexes recompile on each `evaluate` (regions are tiny,
//! evaluate runs at human-perception cadence on readiness polls); cache compiled
//! `Regex`es per rule if a profiler ever flags it. The `prompt_box_body` region
//! and `skip_state_update`/priority semantics are tuned against the reference design,
//! not yet against a live claude TUI (E6.3's job).

use crate::readiness::ScreenView;
use regex::Regex;
use std::path::Path;

/// Max nesting depth for a [`Gate`] tree. A pathological manifest (deeply nested
/// `all`/`any`/`not`) is refused while building the [`Gate`] so `evaluate`'s
/// recursion is bounded. 16 is far past any real rule (the reference's deepest is ~3).
///
/// Note: this caps OUR tree-walk, not `toml::from_str`, which builds the nested
/// `toml::Value` first. For locally-authored (trusted) manifests that is fine;
/// when remote/cached resolution lands (the logged fast-follow) the input must be
/// nesting-bounded BEFORE `toml::from_str`. Tracked as a carveout.
const MAX_GATE_DEPTH: usize = 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest is not valid TOML: {0}")]
    Toml(String),
    #[error("manifest io error for {path}: {detail}")]
    Io { path: String, detail: String },
    #[error("rule {rule}: missing or wrong-typed field '{field}'")]
    Field { rule: String, field: String },
    #[error("rule {rule}: unknown region selector '{region}'")]
    UnknownRegion { rule: String, region: String },
    #[error("rule {rule}: bad regex '{pattern}': {detail}")]
    BadRegex {
        rule: String,
        pattern: String,
        detail: String,
    },
    #[error("rule {rule}: gate nested deeper than {max}", max = MAX_GATE_DEPTH)]
    GateTooDeep { rule: String },
    #[error(
        "rule {rule}: gate table must have exactly one of contains/regex/line_regex/all/any/not"
    )]
    BadGate { rule: String },
}

/// A text region of the screen a rule's gate is matched against. `osc_title` /
/// `osc_progress` read the OSC-captured strings (which survive scrollback/wrap/
/// resize); the rest read the grid text. The v1 set is the design's recommended
/// minimum; `after_last_horizontal_rule` / `after_last_prompt_marker` are
/// deferred until a rule needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// The whole visible screen (scrollback already trimmed by the snapshot).
    WholeRecent,
    /// The last `N` non-empty lines, joined by `\n`. Where a CLI draws its
    /// composer + status bar; scopes a match away from scrollback.
    BottomNonEmptyLines(usize),
    /// The body of the last box-drawn input box (claude's composer). Empty when
    /// no box is on screen.
    PromptBoxBody,
    /// The latest OSC window title (OSC 0/2). Empty when none captured.
    OscTitle,
    /// The latest OSC 9;4 progress payload. Empty when none captured.
    OscProgress,
}

impl Region {
    /// Parse a region selector string. `rule` only seasons the error.
    fn parse(s: &str, rule: &str) -> Result<Region, ManifestError> {
        if let Some(arg) = s
            .strip_prefix("bottom_non_empty_lines(")
            .and_then(|r| r.strip_suffix(')'))
        {
            let n = arg
                .trim()
                .parse::<usize>()
                .map_err(|_| ManifestError::Field {
                    rule: rule.to_string(),
                    field: "region (bottom_non_empty_lines arg)".to_string(),
                })?;
            if n == 0 {
                // bottom(0) is an empty region: a degenerate rule that never
                // fires. Reject it rather than silently never-match (fail closed).
                return Err(ManifestError::Field {
                    rule: rule.to_string(),
                    field: "region (bottom_non_empty_lines arg must be > 0)".to_string(),
                });
            }
            return Ok(Region::BottomNonEmptyLines(n));
        }
        match s {
            "whole_recent" => Ok(Region::WholeRecent),
            "prompt_box_body" => Ok(Region::PromptBoxBody),
            "osc_title" => Ok(Region::OscTitle),
            "osc_progress" => Ok(Region::OscProgress),
            _ => Err(ManifestError::UnknownRegion {
                rule: rule.to_string(),
                region: s.to_string(),
            }),
        }
    }

    /// Extract this region's text from a screen view. An absent OSC region is the
    /// empty string, so a `contains`/`regex` over it never matches (correct: no
    /// title means no spinner) while `not(...)` over it reads as "vacuously true".
    fn extract(&self, screen: &ScreenView) -> String {
        match self {
            Region::WholeRecent => screen.visible_text.to_string(),
            Region::BottomNonEmptyLines(n) => {
                let nonblank: Vec<&str> = screen
                    .visible_text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                let start = nonblank.len().saturating_sub(*n);
                nonblank[start..].join("\n")
            }
            Region::PromptBoxBody => prompt_box_body(screen.visible_text),
            Region::OscTitle => screen.osc_title.unwrap_or("").to_string(),
            Region::OscProgress => screen.osc_progress.unwrap_or("").to_string(),
        }
    }
}

/// Pull the body out of the last box-drawn input box. claude's composer is a
/// `╭─╮ / │ … │ / ╰─╯` box; the body is the `│`-bordered lines between the last
/// bottom border (`╰`) and its nearest preceding top border (`╭`), with the
/// vertical borders stripped. Returns "" when no complete box is present.
///
/// ponytail: a single-heuristic box finder, tuned to claude's box-drawing glyphs;
/// it does not handle nested boxes or ASCII `+--+` frames, and it does NOT yet
/// distinguish the live composer from a box-drawn TABLE up in scrollback - it just
/// takes the bottommost `╰`/`╭` pair, so a scrollback table can be extracted as
/// stale "prompt body" and let a rule false-match (codex peer P2). Disambiguating
/// composer-vs-scrollback needs ground truth (the box near the status area / on the
/// cursor row) against a live claude TUI; deliberately not guessed here. E6.3's
/// `claude.toml` `live_prompt_box` rule consumes this region, so that
/// disambiguation is its load-bearing follow-up (carveout, pinned when E2 lands).
fn prompt_box_body(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(bottom) = lines.iter().rposition(|l| l.contains('╰')) else {
        return String::new();
    };
    let Some(top) = lines[..bottom].iter().rposition(|l| l.contains('╭')) else {
        return String::new();
    };
    lines[top + 1..bottom]
        .iter()
        .map(|l| l.trim().trim_matches('│').trim().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A recursive boolean predicate over a region's text. Leaf predicates test the
/// region string; `all`/`any`/`not` compose them. Regexes are compiled at parse,
/// so a constructed `Gate` is always valid.
#[derive(Debug, Clone)]
pub enum Gate {
    /// Region contains this substring.
    Contains(String),
    /// Region matches this regex anywhere (use `^`/`$` to anchor).
    Regex(Regex),
    /// Any single line of the region matches this regex.
    LineRegex(Regex),
    /// Every sub-gate matches.
    All(Vec<Gate>),
    /// At least one sub-gate matches.
    Any(Vec<Gate>),
    /// The sub-gate does not match.
    Not(Box<Gate>),
}

impl Gate {
    /// Build a gate from a TOML value. The value must be a table with exactly one
    /// recognized key. `depth` guards against pathological nesting.
    fn parse(v: &toml::Value, rule: &str, depth: usize) -> Result<Gate, ManifestError> {
        if depth > MAX_GATE_DEPTH {
            return Err(ManifestError::GateTooDeep {
                rule: rule.to_string(),
            });
        }
        let table = v.as_table().ok_or_else(|| ManifestError::BadGate {
            rule: rule.to_string(),
        })?;
        if table.len() != 1 {
            return Err(ManifestError::BadGate {
                rule: rule.to_string(),
            });
        }
        let (key, val) = table.iter().next().expect("len checked == 1");
        let compile = |p: &str| {
            Regex::new(p).map_err(|e| ManifestError::BadRegex {
                rule: rule.to_string(),
                pattern: p.to_string(),
                detail: e.to_string(),
            })
        };
        let as_str = || {
            val.as_str().ok_or_else(|| ManifestError::Field {
                rule: rule.to_string(),
                field: format!("gate.{key}"),
            })
        };
        // An empty leaf pattern is fail-open the same way `all = []` is:
        // `"".contains("")` and `Regex::new("")` both match every region, pinning
        // the rule's state on every poll. Reject empty leaves at parse.
        let leaf_str = || {
            let s = as_str()?;
            if s.is_empty() {
                return Err(ManifestError::Field {
                    rule: rule.to_string(),
                    field: format!("gate.{key} (must be non-empty)"),
                });
            }
            Ok(s)
        };
        let as_array = || {
            val.as_array().ok_or_else(|| ManifestError::Field {
                rule: rule.to_string(),
                field: format!("gate.{key}"),
            })
        };
        match key.as_str() {
            "contains" => Ok(Gate::Contains(leaf_str()?.to_string())),
            "regex" => Ok(Gate::Regex(compile(leaf_str()?)?)),
            "line_regex" => Ok(Gate::LineRegex(compile(leaf_str()?)?)),
            "all" => Ok(Gate::All(Self::parse_children(as_array()?, rule, depth)?)),
            "any" => Ok(Gate::Any(Self::parse_children(as_array()?, rule, depth)?)),
            "not" => Ok(Gate::Not(Box::new(Gate::parse(val, rule, depth + 1)?))),
            _ => Err(ManifestError::BadGate {
                rule: rule.to_string(),
            }),
        }
    }

    fn parse_children(
        arr: &[toml::Value],
        rule: &str,
        depth: usize,
    ) -> Result<Vec<Gate>, ManifestError> {
        // An empty `all`/`any` is fail-open: `all([])` matches every screen
        // (vacuous truth), so a high-priority rule with `all = []` would pin its
        // state on every poll. Reject it at parse rather than mis-fire silently.
        if arr.is_empty() {
            return Err(ManifestError::BadGate {
                rule: rule.to_string(),
            });
        }
        arr.iter()
            .map(|child| Gate::parse(child, rule, depth + 1))
            .collect()
    }

    /// Evaluate against a region's text.
    fn matches(&self, text: &str) -> bool {
        match self {
            Gate::Contains(s) => text.contains(s.as_str()),
            Gate::Regex(re) => re.is_match(text),
            Gate::LineRegex(re) => text.lines().any(|l| re.is_match(l)),
            Gate::All(gs) => gs.iter().all(|g| g.matches(text)),
            Gate::Any(gs) => gs.iter().any(|g| g.matches(text)),
            Gate::Not(g) => !g.matches(text),
        }
    }
}

/// One detection rule: when `gate` matches `region`'s text, the agent is in
/// `state`. `priority` arbitrates between simultaneously-matching rules (highest
/// wins). `skip_state_update` marks a rule whose match means "hold the current
/// state, don't update it" - e.g. claude's ctrl+o transcript pager, which must
/// not flip a working agent to idle.
#[derive(Debug, Clone)]
pub struct ManifestRule {
    pub id: String,
    pub state: String,
    pub priority: i32,
    pub region: Region,
    pub skip_state_update: bool,
    pub gate: Gate,
}

impl ManifestRule {
    fn parse(v: &toml::Value) -> Result<ManifestRule, ManifestError> {
        // id is read first so every later error can name the rule.
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| ManifestError::Field {
                rule: "<unnamed>".to_string(),
                field: "id".to_string(),
            })?
            .to_string();
        if id.trim().is_empty() {
            // The id seasons every error and rides in the Verdict; an empty one
            // makes both useless. Require it non-blank.
            return Err(ManifestError::Field {
                rule: "<unnamed>".to_string(),
                field: "id (must be non-empty)".to_string(),
            });
        }
        let str_field = |f: &str| {
            v.get(f)
                .and_then(|x| x.as_str())
                .ok_or_else(|| ManifestError::Field {
                    rule: id.clone(),
                    field: f.to_string(),
                })
        };
        let state = str_field("state")?.to_string();
        let priority_i64 = v
            .get("priority")
            .and_then(|x| x.as_integer())
            .ok_or_else(|| ManifestError::Field {
                rule: id.clone(),
                field: "priority".to_string(),
            })?;
        // TOML integers are i64; `as i32` would silently wrap a too-big priority
        // and corrupt arbitration. Reject out-of-range rather than truncate.
        let priority = i32::try_from(priority_i64).map_err(|_| ManifestError::Field {
            rule: id.clone(),
            field: "priority (out of i32 range)".to_string(),
        })?;
        let region = Region::parse(str_field("region")?, &id)?;
        // Present-but-wrong-type (e.g. `skip_state_update = "true"`) must error,
        // not silently read as false and swallow an authoring typo.
        let skip_state_update = match v.get("skip_state_update") {
            None => false,
            Some(x) => x.as_bool().ok_or_else(|| ManifestError::Field {
                rule: id.clone(),
                field: "skip_state_update (must be a boolean)".to_string(),
            })?,
        };
        let gate_val = v.get("gate").ok_or_else(|| ManifestError::Field {
            rule: id.clone(),
            field: "gate".to_string(),
        })?;
        let gate = Gate::parse(gate_val, &id, 0)?;
        // Reject unknown keys: a typo like `skip_state_updates = true` would
        // otherwise parse fine and silently drop the real flag, changing
        // arbitration. Fail closed instead (matches the gate's one-key rule).
        if let Some(table) = v.as_table() {
            const ALLOWED: &[&str] = &[
                "id",
                "state",
                "priority",
                "region",
                "skip_state_update",
                "gate",
            ];
            if let Some(unknown) = table.keys().find(|k| !ALLOWED.contains(&k.as_str())) {
                return Err(ManifestError::Field {
                    rule: id.clone(),
                    field: format!("unknown key '{unknown}'"),
                });
            }
        }
        Ok(ManifestRule {
            id,
            state,
            priority,
            region,
            skip_state_update,
            gate,
        })
    }
}

/// The verdict of evaluating a manifest against a screen: the matching rule's
/// id, the state it asserts, and whether the caller should hold the current
/// state instead of applying `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict<'a> {
    pub rule_id: &'a str,
    pub state: &'a str,
    pub skip_state_update: bool,
}

/// A parsed agent detection manifest: an engine-version floor plus the rules,
/// pre-sorted highest-priority-first so [`evaluate`](Manifest::evaluate) is a
/// linear scan that returns on the first match.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Minimum engine version a (future remote) manifest may demand. Parsed and
    /// stored now though unused in v1 (design: "Engine-version field now even if
    /// unused, so a later remote manifest can gate"). Defaults to 0.
    pub min_engine_version: u32,
    /// Rules sorted by `priority` descending; ties keep TOML order (stable sort).
    /// Private so the sort invariant `evaluate` relies on can only be established
    /// by [`parse`](Manifest::parse); read via [`rules`](Manifest::rules).
    rules: Vec<ManifestRule>,
}

impl Manifest {
    /// Parse a manifest TOML. Fails closed: a bad regex, unknown region, or
    /// over-deep gate is a parse error naming the offending rule, never a
    /// silently-dropped rule.
    pub fn parse(s: &str) -> Result<Manifest, ManifestError> {
        let root: toml::Value =
            toml::from_str(s).map_err(|e| ManifestError::Toml(e.to_string()))?;
        // Reject unknown root keys (typo like `min_engine_versions`). A later
        // format bump is gated by `min_engine_version`, not by tolerating
        // unknown keys, so fail closed in v1.
        if let Some(table) = root.as_table() {
            if let Some(unknown) = table
                .keys()
                .find(|k| !matches!(k.as_str(), "min_engine_version" | "rule"))
            {
                return Err(ManifestError::Field {
                    rule: "<root>".to_string(),
                    field: format!("unknown key '{unknown}'"),
                });
            }
        }
        // Absent -> 0. Present-but-wrong-type or negative is a malformed manifest,
        // not a silent default-to-0 (fail closed, matching the per-rule fields).
        let min_engine_version = match root.get("min_engine_version") {
            None => 0,
            Some(v) => v
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| ManifestError::Field {
                    rule: "<root>".to_string(),
                    field: "min_engine_version (must be a non-negative integer)".to_string(),
                })?,
        };
        let mut rules = match root.get("rule") {
            Some(v) => v
                .as_array()
                .ok_or_else(|| ManifestError::Field {
                    rule: "<root>".to_string(),
                    field: "rule (must be an array of tables)".to_string(),
                })?
                .iter()
                .map(ManifestRule::parse)
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        // Highest priority first; stable so equal-priority rules keep file order.
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));
        Ok(Manifest {
            min_engine_version,
            rules,
        })
    }

    /// The parsed rules, highest-priority-first. Read-only: the sort invariant is
    /// owned by [`parse`](Manifest::parse).
    pub fn rules(&self) -> &[ManifestRule] {
        &self.rules
    }

    /// Return the highest-priority rule whose gate matches the screen, or `None`
    /// when no rule matches (the caller decides what an undetected state means -
    /// the engine never guesses).
    pub fn evaluate(&self, screen: &ScreenView) -> Option<Verdict<'_>> {
        self.rules.iter().find_map(|rule| {
            let text = rule.region.extract(screen);
            rule.gate.matches(&text).then_some(Verdict {
                rule_id: &rule.id,
                state: &rule.state,
                skip_state_update: rule.skip_state_update,
            })
        })
    }
}

/// The detection manifest compiled into the binary for a known agent (E6.3).
/// Returns `None` for an unknown agent - the caller fails loud rather than
/// guessing a manifest (mirrors `readiness.rs`'s Open Question #9: no
/// fail-open default).
pub fn bundled_manifest(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some(include_str!("manifests/claude.toml")),
        "codex" => Some(include_str!("manifests/codex.toml")),
        "gemini" => Some(include_str!("manifests/gemini.toml")),
        // x-8f7f: agy (hosted, US1) + opencode (staged/inert until x-51f6, US2).
        "agy" => Some(include_str!("manifests/agy.toml")),
        "opencode" => Some(include_str!("manifests/opencode.toml")),
        // x-83e7: full-roster roster. All staged/inert - none has a provider
        // host yet (no build_pane_argv arm), so each is bundled-but-dormant like
        // opencode. Adapted from the reference manifests per manifests/ADAPTING.md.
        // "copilot" resolves github-copilot.toml, mirroring the reference's own mapping.
        // antigravity is intentionally absent: the reference antigravity manifest is the
        // agy harness (id "agy"), already covered by agy.toml above.
        "amp" => Some(include_str!("manifests/amp.toml")),
        "cline" => Some(include_str!("manifests/cline.toml")),
        "cursor" => Some(include_str!("manifests/cursor.toml")),
        "devin" => Some(include_str!("manifests/devin.toml")),
        "droid" => Some(include_str!("manifests/droid.toml")),
        "copilot" => Some(include_str!("manifests/github-copilot.toml")),
        "grok" => Some(include_str!("manifests/grok.toml")),
        "hermes" => Some(include_str!("manifests/hermes.toml")),
        "kilo" => Some(include_str!("manifests/kilo.toml")),
        "kimi" => Some(include_str!("manifests/kimi.toml")),
        "kiro" => Some(include_str!("manifests/kiro.toml")),
        "pi" => Some(include_str!("manifests/pi.toml")),
        "qodercli" => Some(include_str!("manifests/qodercli.toml")),
        _ => None,
    }
}

/// Resolve and parse an agent's manifest. v1 resolution chain (design: bundled +
/// local override; remote/cached deferred): a readable `<agent>.toml` in
/// `override_dir` wins over the bundled copy, so an operator can hand-author a
/// rule file without a rebuild.
///
/// Returns `None` when no manifest exists for `agent` (unknown agent, no
/// override) - the caller decides what "no manifest" means and never guesses.
/// `Some(Err(..))` is a present-but-malformed manifest (the override or bundled
/// TOML failed to parse), surfaced verbatim so a bad hand edit fails loud
/// instead of silently falling back.
pub fn load_manifest(
    agent: &str,
    override_dir: Option<&Path>,
) -> Option<Result<Manifest, ManifestError>> {
    if let Some(dir) = override_dir {
        let path = dir.join(format!("{agent}.toml"));
        // A PRESENT override file is honoured as the operator's intent and fails
        // loud: a parse-bad TOML surfaces ManifestError::Toml, and a present file
        // that won't read (invalid UTF-8, permission-denied, lookup error)
        // surfaces ManifestError::Io. ONLY a genuinely absent override (a
        // NotFound read error) falls through to bundled. We match on the read
        // error kind rather than pre-checking is_file(), because is_file()
        // collapses every metadata error (permission, symlink loop) to false and
        // would silently fall back to bundled on a real error (codex peer P2).
        match std::fs::read_to_string(&path) {
            Ok(text) => return Some(Manifest::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Some(Err(ManifestError::Io {
                    path: path.display().to_string(),
                    detail: e.to_string(),
                }))
            }
        }
    }
    bundled_manifest(agent).map(Manifest::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(text: &str) -> ScreenView<'_> {
        ScreenView {
            visible_text: text,
            cursor_row: 0,
            cursor_col: 0,
            osc_title: None,
            osc_progress: None,
        }
    }

    fn view_title<'a>(text: &'a str, title: &'a str) -> ScreenView<'a> {
        ScreenView {
            visible_text: text,
            cursor_row: 0,
            cursor_col: 0,
            osc_title: Some(title),
            osc_progress: None,
        }
    }

    #[test]
    fn parses_fields_and_sorts_by_priority_desc() {
        let m = Manifest::parse(
            r#"
            min_engine_version = 2
            [[rule]]
            id = "low"
            state = "idle"
            priority = 10
            region = "whole_recent"
            gate = { contains = "x" }
            [[rule]]
            id = "high"
            state = "working"
            priority = 100
            region = "whole_recent"
            gate = { contains = "y" }
            "#,
        )
        .unwrap();
        assert_eq!(m.min_engine_version, 2);
        assert_eq!(m.rules().len(), 2);
        assert_eq!(m.rules()[0].id, "high", "highest priority sorts first");
        assert_eq!(m.rules()[1].id, "low");
    }

    #[test]
    fn min_engine_version_defaults_to_zero() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "idle"
            priority = 1
            region = "whole_recent"
            gate = { contains = "x" }
            "#,
        )
        .unwrap();
        assert_eq!(m.min_engine_version, 0);
    }

    // AC-E6-5: highest-priority match wins. A "yes" in scrollback must NOT fake a
    // permission prompt because the live-region rule out-prioritizes it.
    #[test]
    fn highest_priority_match_wins_over_scrollback() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "scrollback_yes"
            state = "blocked"
            priority = 100
            region = "whole_recent"
            gate = { contains = "yes" }
            [[rule]]
            id = "live_prompt"
            state = "idle"
            priority = 900
            region = "bottom_non_empty_lines(1)"
            gate = { line_regex = "^\\s*❯" }
            "#,
        )
        .unwrap();
        // "yes" is up in scrollback; the live composer shows the idle prompt.
        let screen = "I said yes earlier\nlots of reply text\n  ❯ ";
        let v = m.evaluate(&view(screen)).unwrap();
        assert_eq!(v.state, "idle", "live-region rule beats scrollback match");
        assert_eq!(v.rule_id, "live_prompt");
    }

    // AC-E6-2 (engine half): a braille-spinner title badges working from the
    // title alone, with the grid showing only scrollback (no glyph in the grid).
    #[test]
    fn osc_title_braille_spinner_badges_working_from_title_alone() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "osc_title_working"
            state = "working"
            priority = 1100
            region = "osc_title"
            gate = { regex = "^[\\x{2800}-\\x{28FF}]" }
            "#,
        )
        .unwrap();
        // Grid is pure scrollback (no spinner); the title carries U+280B.
        let screen = view_title("old output\nmore scrollback\n", "\u{280b} Compiling");
        let v = m.evaluate(&screen).unwrap();
        assert_eq!(v.state, "working");
        // No title -> the rule does not fire (engine never guesses).
        assert!(m.evaluate(&view("old output")).is_none());
    }

    // AC-E6-3: skip_state_update on a transcript-viewer rule keeps a working
    // agent from flipping to idle when the ctrl+o pager is open.
    #[test]
    fn skip_state_update_flag_is_carried_through() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "transcript_viewer"
            state = "idle"
            priority = 1000
            region = "bottom_non_empty_lines(3)"
            skip_state_update = true
            gate = { contains = "(END)" }
            "#,
        )
        .unwrap();
        let v = m.evaluate(&view("scrollback\nmore\n(END)")).unwrap();
        assert!(v.skip_state_update, "pager rule must not update state");
    }

    #[test]
    fn gate_all_any_not_compose() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "blocked_form"
            state = "blocked"
            priority = 980
            region = "whole_recent"
            gate = { all = [ { contains = "enter to select" }, { contains = "esc to cancel" }, { not = { contains = "esc to interrupt" } } ] }
            "#,
        )
        .unwrap();
        // all three sub-gates satisfied
        assert!(m
            .evaluate(&view("press enter to select, esc to cancel"))
            .is_some());
        // missing "esc to cancel" -> all() fails
        assert!(m.evaluate(&view("enter to select something")).is_none());
        // the not() clause: an interrupt hint present -> blocked rule must NOT fire
        assert!(m
            .evaluate(&view("enter to select, esc to cancel, esc to interrupt"))
            .is_none());
    }

    #[test]
    fn any_gate_matches_on_one() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "perm"
            state = "blocked"
            priority = 850
            region = "whole_recent"
            gate = { any = [ { contains = "do you want to proceed?" }, { contains = "1. Yes" } ] }
            "#,
        )
        .unwrap();
        assert!(m.evaluate(&view("1. Yes\n2. No")).is_some());
        assert!(m.evaluate(&view("nothing relevant")).is_none());
    }

    #[test]
    fn region_prompt_box_body_extracts_box_interior() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "live_prompt_box"
            state = "idle"
            priority = 950
            region = "prompt_box_body"
            gate = { regex = "❯" }
            "#,
        )
        .unwrap();
        // A "❯" in scrollback above the box must not count; only the box body does.
        let screen = "❯ earlier command in history\n\
                      ╭──────────────╮\n\
                      │ ❯ type here  │\n\
                      ╰──────────────╯";
        assert!(m.evaluate(&view(screen)).is_some());
        // No box on screen -> empty region -> no match.
        assert!(m.evaluate(&view("just text, no box")).is_none());
    }

    #[test]
    fn region_osc_progress_reads_progress_payload() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "progressing"
            state = "working"
            priority = 500
            region = "osc_progress"
            gate = { regex = "^4;" }
            "#,
        )
        .unwrap();
        let screen = ScreenView {
            visible_text: "anything",
            cursor_row: 0,
            cursor_col: 0,
            osc_title: None,
            osc_progress: Some("4;1;50"),
        };
        assert_eq!(m.evaluate(&screen).unwrap().state, "working");
    }

    #[test]
    fn no_rule_matches_returns_none() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "idle"
            priority = 1
            region = "whole_recent"
            gate = { contains = "zzz" }
            "#,
        )
        .unwrap();
        assert!(m.evaluate(&view("nothing here")).is_none());
    }

    #[test]
    fn empty_manifest_parses_to_no_rules() {
        let m = Manifest::parse("min_engine_version = 1").unwrap();
        assert!(m.rules().is_empty());
        assert!(m.evaluate(&view("anything")).is_none());
    }

    #[test]
    fn bad_regex_is_a_parse_error_not_a_silent_drop() {
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "broken"
            state = "x"
            priority = 1
            region = "whole_recent"
            gate = { regex = "(" }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::BadRegex { rule, .. } if rule == "broken"));
    }

    #[test]
    fn unknown_region_is_a_parse_error() {
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "the_moon"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::UnknownRegion { region, .. } if region == "the_moon"));
    }

    #[test]
    fn missing_required_field_names_the_rule() {
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            priority = 1
            region = "whole_recent"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ManifestError::Field { rule, field } if rule == "r" && field == "state")
        );
    }

    #[test]
    fn multi_key_gate_table_is_rejected() {
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "whole_recent"
            gate = { contains = "a", regex = "b" }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::BadGate { rule } if rule == "r"));
    }

    #[test]
    fn over_deep_gate_is_refused() {
        // Build a gate nested past MAX_GATE_DEPTH with chained `not`s.
        let mut gate = "{ contains = \"x\" }".to_string();
        for _ in 0..(MAX_GATE_DEPTH + 2) {
            gate = format!("{{ not = {gate} }}");
        }
        let toml = format!(
            "[[rule]]\nid = \"deep\"\nstate = \"x\"\npriority = 1\nregion = \"whole_recent\"\ngate = {gate}\n"
        );
        let err = Manifest::parse(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::GateTooDeep { rule } if rule == "deep"));
    }

    #[test]
    fn bottom_non_empty_lines_scopes_to_tail() {
        let m = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "hit"
            priority = 1
            region = "bottom_non_empty_lines(2)"
            gate = { contains = "needle" }
            "#,
        )
        .unwrap();
        // needle is on line 1 of 4 non-empty lines; bottom(2) must not see it.
        let screen = "needle up here\n\nfiller\nmore filler\nlast line";
        assert!(m.evaluate(&view(screen)).is_none());
        // needle in the last two lines -> match.
        assert!(m.evaluate(&view("filler\nfiller\nneedle\nlast")).is_some());
    }

    #[test]
    fn empty_composite_gate_is_rejected_not_fail_open() {
        // `all = []` is vacuously true and would pin its state on every screen.
        // Both empty `all` and empty `any` must be parse errors.
        for body in ["all = []", "any = []"] {
            let err = Manifest::parse(&format!(
                "[[rule]]\nid = \"r\"\nstate = \"x\"\npriority = 1\nregion = \"whole_recent\"\ngate = {{ {body} }}\n"
            ))
            .unwrap_err();
            assert!(
                matches!(err, ManifestError::BadGate { rule } if rule == "r"),
                "{body} should be rejected"
            );
        }
    }

    #[test]
    fn bottom_non_empty_lines_zero_is_rejected() {
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "bottom_non_empty_lines(0)"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::Field { rule, .. } if rule == "r"));
    }

    #[test]
    fn malformed_scalar_fields_are_rejected_not_coerced() {
        // priority out of i32 range -> error (not a silent wrap).
        let big = i64::from(i32::MAX) + 1;
        let err = Manifest::parse(&format!(
            "[[rule]]\nid = \"r\"\nstate = \"x\"\npriority = {big}\nregion = \"whole_recent\"\ngate = {{ contains = \"x\" }}\n"
        ))
        .unwrap_err();
        assert!(matches!(err, ManifestError::Field { field, .. } if field.starts_with("priority")));

        // skip_state_update present but not a bool -> error (not silent false).
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "whole_recent"
            skip_state_update = "yes"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ManifestError::Field { field, .. } if field.starts_with("skip_state_update"))
        );

        // min_engine_version present but wrong type -> error (not silent 0).
        let err = Manifest::parse(
            r#"
            min_engine_version = "two"
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "whole_recent"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ManifestError::Field { field, .. } if field.starts_with("min_engine_version"))
        );
    }

    #[test]
    fn empty_leaf_gate_pattern_is_rejected_not_fail_open() {
        // `contains = ""` / `regex = ""` / `line_regex = ""` each match every
        // region; reject them like an empty `all = []` (codex peer P2).
        for leaf in [r#"contains = """#, r#"regex = """#, r#"line_regex = """#] {
            let err = Manifest::parse(&format!(
                "[[rule]]\nid = \"r\"\nstate = \"x\"\npriority = 1\nregion = \"whole_recent\"\ngate = {{ {leaf} }}\n"
            ))
            .unwrap_err();
            assert!(
                matches!(err, ManifestError::Field { rule, .. } if rule == "r"),
                "{leaf} should be rejected"
            );
        }
    }

    #[test]
    fn unknown_rule_and_root_keys_are_rejected() {
        // A typo'd rule key (`skip_state_updates`) would silently drop the real
        // flag; reject it (codex peer P2).
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "whole_recent"
            skip_state_updates = true
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ManifestError::Field { rule, field } if rule == "r" && field.contains("unknown key"))
        );

        // A typo'd root key is rejected too.
        let err = Manifest::parse(
            r#"
            min_engine_versions = 1
            [[rule]]
            id = "r"
            state = "x"
            priority = 1
            region = "whole_recent"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(err, ManifestError::Field { rule, field } if rule == "<root>" && field.contains("unknown key"))
        );
    }

    #[test]
    fn empty_id_is_rejected() {
        let err = Manifest::parse(
            r#"
            [[rule]]
            id = ""
            state = "x"
            priority = 1
            region = "whole_recent"
            gate = { contains = "x" }
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::Field { field, .. } if field.starts_with("id")));
    }

    // ---- E6.3: bundled rule files (claude/codex/gemini) ----

    use crate::readiness::{CodexReadinessDetector, GeminiReadinessDetector, ReadinessDetector};

    fn bundled(agent: &str) -> Manifest {
        Manifest::parse(bundled_manifest(agent).expect("bundled manifest exists"))
            .expect("bundled manifest parses")
    }

    /// Derive the old boolean readiness from a manifest verdict: ready only when
    /// the live state is `idle` and the rule did not ask us to hold state. This
    /// is the mapping the daemon badge will use when E2 wires evaluate() in.
    fn manifest_ready(m: &Manifest, screen: &ScreenView) -> bool {
        matches!(m.evaluate(screen), Some(v) if v.state == "idle" && !v.skip_state_update)
    }

    #[test]
    fn bundled_manifests_all_parse() {
        // Every bundled agent must parse, carry rules, and evaluate against a
        // synthetic view without panicking (x-83e7 AC-happy). This is the
        // parse-coverage guard the domain pitfall calls for: a leftover reference-
        // only key (unknown region/field/root key) fails loud here, NAMING the
        // file (x-83e7 AC-error) rather than silently shipping a dead manifest.
        // x-8f7f added agy + opencode; x-83e7 grew the roster to full-roster parity.
        let synthetic = view("some scrollback\nesc to interrupt\n\u{276f} ");
        for agent in [
            "claude", "codex", "gemini", "agy", "opencode", // pre-x-83e7
            "amp", "cline", "cursor", "devin", "droid", "copilot", "grok", "hermes", "kilo",
            "kimi", "kiro", "pi", "qodercli", // x-83e7
        ] {
            let src = bundled_manifest(agent).unwrap_or_else(|| panic!("{agent} is bundled"));
            let m = Manifest::parse(src)
                .unwrap_or_else(|e| panic!("{agent}.toml failed to parse: {e:?}"));
            assert!(!m.rules().is_empty(), "{agent}.toml has rules");
            // Must not panic (regexes compiled at parse; this exercises evaluate).
            let _ = m.evaluate(&synthetic);
        }
        // A genuinely-unhosted harness still resolves to None (the fail-loud
        // guard, mirrors readiness OQ#9: no fail-open default). aider is a real
        // coding CLI we deliberately do not bundle a manifest for.
        assert!(bundled_manifest("aider").is_none(), "unknown agent -> None");
    }

    // AC-E6-4: codex/gemini ported to TOML reproduce the hardcoded
    // CodexReadinessDetector/GeminiReadinessDetector decisions on the exact
    // readiness.rs test inputs, INCLUDING gemini's "Waiting for auth" false-ready.
    #[test]
    fn ac_e6_4_codex_gemini_toml_match_hardcoded_detectors() {
        let codex_m = bundled("codex");
        let gemini_m = bundled("gemini");
        // (input, expected ready) - mirrors readiness.rs's detector tests.
        let cases: &[(&str, bool)] = &[
            ("codex 0.130\n\n  build feature X\n\u{276f} ", true), // idle prompt
            ("running tool...\nEsc to interrupt\n\u{276f}", false), // busy beats glyph
            ("loading a 5000 byte banner of text", false),         // no glyph -> not ready
            ("Waiting for auth...\n\u{276f}", false),              // gemini false-ready trap
            ("Gemini ready\n\u{203a} ", true),                     // › idle glyph
            // "Working"/"Thinking" up in scrollback must NOT block (Codex P1).
            (
                "I am Working on the Thinking task you asked about.\n\
                 Here is a long reply that mentions Working again.\n\
                 filler line\nanother filler\n\u{276f} ",
                true,
            ),
        ];
        for (text, want) in cases {
            let trimmed = text.trim_end();
            let screen = view(trimmed);
            assert_eq!(
                manifest_ready(&codex_m, &screen),
                *want,
                "codex.toml readiness mismatch for {trimmed:?}"
            );
            // Cross-check against the real hardcoded detector: the TOML must
            // agree with the Rust it replaces, not just with `want`.
            assert_eq!(
                manifest_ready(&codex_m, &screen),
                CodexReadinessDetector.is_ready(&screen).unwrap(),
                "codex.toml diverges from CodexReadinessDetector for {trimmed:?}"
            );
            assert_eq!(
                manifest_ready(&gemini_m, &screen),
                GeminiReadinessDetector.is_ready(&screen).unwrap(),
                "gemini.toml diverges from GeminiReadinessDetector for {trimmed:?}"
            );
        }
    }

    // AC-E6-2: claude.toml's braille-spinner osc_title_working rule badges
    // `working` from the title alone, with the grid showing only scrollback.
    #[test]
    fn ac_e6_2_claude_osc_title_spinner_badges_working() {
        let m = bundled("claude");
        // Grid is pure scrollback (no spinner glyph); the title carries U+280B.
        let screen = view_title("old output\nmore scrollback\n", "\u{280b} Compiling");
        let v = m.evaluate(&screen).expect("spinner title matches");
        assert_eq!(v.state, "working");
        assert_eq!(v.rule_id, "osc_title_working");
        // No title at all -> the title rule cannot fire (engine never guesses).
        assert!(m
            .evaluate(&view("old output\nmore scrollback"))
            .is_none_or(|v| v.rule_id != "osc_title_working"));
    }

    // AC-E6-3: skip_state_update on claude.toml's transcript_viewer keeps a
    // ctrl+o transcript pager from flipping the badge to idle.
    #[test]
    fn ac_e6_3_claude_transcript_viewer_holds_state() {
        let m = bundled("claude");
        let v = m
            .evaluate(&view("scrollback line\nmore scrollback\n(END)"))
            .expect("transcript pager marker matches");
        assert_eq!(v.rule_id, "transcript_viewer");
        assert!(
            v.skip_state_update,
            "pager rule must hold state, not set idle"
        );
    }

    // AC-E6-5: highest-priority match wins. A claude whose grid shows an idle
    // composer box still badges `working` when the OSC title spinner is up,
    // because osc_title_working (1100) out-prioritizes live_prompt_box (950) -
    // the title is the authority a scraped grid cannot fake.
    #[test]
    fn ac_e6_5_claude_title_spinner_outranks_idle_grid_box() {
        let m = bundled("claude");
        let grid = "\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n\
                    \u{2502} \u{276f} type here \u{2502}\n\
                    \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}";
        // Sanity: with no title, the idle composer box wins -> idle.
        let v_idle = m.evaluate(&view(grid)).expect("idle box matches");
        assert_eq!(v_idle.state, "idle");
        assert_eq!(v_idle.rule_id, "live_prompt_box");
        // With the spinner title up, working out-prioritizes the same idle box.
        let v_working = m
            .evaluate(&view_title(grid, "\u{280b} Working"))
            .expect("spinner title matches");
        assert_eq!(v_working.state, "working");
        assert_eq!(v_working.rule_id, "osc_title_working");
    }

    // A live permission prompt outranks an idle composer box drawn beneath it:
    // badging `idle` while a prompt is up would be a false-ready (forbidden).
    #[test]
    fn ac_e6_5_claude_permission_prompt_outranks_idle_box() {
        let m = bundled("claude");
        // A permission prompt with the composer box still rendered below it.
        let screen = "do you want to proceed?\n\
                      1. Yes\n\
                      2. No\n\
                      \u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n\
                      \u{2502} \u{276f} type \u{2502}\n\
                      \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}";
        let v = m.evaluate(&view(screen)).expect("a rule matches");
        assert_eq!(
            v.state, "blocked",
            "permission prompt must beat the idle box"
        );
        assert_eq!(v.rule_id, "permission_prompt");
    }

    #[test]
    fn load_manifest_prefers_override_then_bundled_then_none() {
        // No override dir -> bundled.
        let m = load_manifest("claude", None)
            .expect("known agent")
            .expect("parses");
        assert!(!m.rules().is_empty());
        // Unknown agent, no override -> None (caller fails loud). hermes is now
        // bundled (x-83e7 full-roster parity), so use aider (a real coding CLI we
        // deliberately do not bundle a manifest for).
        assert!(load_manifest("aider", None).is_none());

        // A readable <agent>.toml override wins over the bundled copy.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("codex.toml"),
            "[[rule]]\nid = \"override_only\"\nstate = \"idle\"\npriority = 1\nregion = \"whole_recent\"\ngate = { contains = \"OVR\" }\n",
        )
        .unwrap();
        let m = load_manifest("codex", Some(dir.path()))
            .expect("override present")
            .expect("override parses");
        assert_eq!(m.rules().len(), 1);
        assert_eq!(m.rules()[0].id, "override_only");
        // A missing override file for another agent falls through to bundled.
        let m = load_manifest("gemini", Some(dir.path()))
            .expect("falls back to bundled")
            .expect("parses");
        assert!(m.rules().iter().any(|r| r.id == "idle_prompt"));
        // A present-but-malformed override surfaces the parse error (no silent
        // fallback to bundled - a bad hand edit must fail loud).
        std::fs::write(dir.path().join("claude.toml"), "this = is = not = toml").unwrap();
        assert!(matches!(
            load_manifest("claude", Some(dir.path())),
            Some(Err(ManifestError::Toml(_)))
        ));
        // A present override that won't read (invalid UTF-8) also fails loud as
        // an Io error, NOT a silent fallback to bundled (gemini review).
        std::fs::write(dir.path().join("gemini.toml"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(matches!(
            load_manifest("gemini", Some(dir.path())),
            Some(Err(ManifestError::Io { .. }))
        ));
    }

    // AC1-HP (x-8f7f): agy's manifest is authored from AgyReadinessDetector
    // (agy wraps Gemini, shares prompt_ready), so it badges idle/working/blocked
    // on the same conditions gemini does, with the never-false-ready bias
    // (auth_wall 980 > busy 900 > idle_prompt 100).
    #[test]
    fn x8f7f_agy_manifest_evaluates_idle_working_blocked() {
        let m = bundled("agy");
        assert_eq!(
            m.evaluate(&view("agy 1.0\n\u{276f} ")).unwrap().state,
            "idle"
        );
        assert_eq!(
            m.evaluate(&view("running tool...\nesc to interrupt\n\u{276f}"))
                .unwrap()
                .state,
            "working", // busy (900) beats the idle glyph (100)
        );
        assert_eq!(
            m.evaluate(&view("Waiting for auth...\n\u{276f}"))
                .unwrap()
                .state,
            "blocked", // auth_wall (980) is the never-false-ready guard
        );
    }

    // AC2-HP + AC2-EDGE (x-8f7f): opencode's reference manifest, translated per
    // ADAPTING.md, matches the same screens the reference's rules match - including the
    // multi-key AND permission rule whose nesting is preserved under one gate.
    #[test]
    fn x8f7f_opencode_manifest_matches_reference_screens() {
        let m = bundled("opencode");
        // Simple blocked marker.
        assert_eq!(
            m.evaluate(&view("△ Permission required")).unwrap().state,
            "blocked",
        );
        // Both working markers.
        assert_eq!(
            m.evaluate(&view("thinking\nesc to interrupt"))
                .unwrap()
                .state,
            "working",
        );
        assert_eq!(
            m.evaluate(&view("progress \u{25a0}\u{25a0}\u{25a0}\u{25a0}\u{25a0}"))
                .unwrap()
                .state,
            "working", // progress-bar regex (■|⬝){4,}
        );
        // AC2-EDGE: the nested any/all permission branch (esc dismiss AND a
        // confirm hint AND a select hint) still resolves to blocked.
        assert_eq!(
            m.evaluate(&view(
                "esc dismiss   enter confirm   \u{2191}\u{2193} select"
            ))
            .unwrap()
            .state,
            "blocked",
        );
        // A bare model reply that merely mentions none of the markers -> no rule
        // fires (the engine never guesses).
        assert!(m.evaluate(&view("here is your answer")).is_none());
    }

    // AC2-ERR (x-8f7f): an adaptation that leaves a unknown source key in the TOML
    // fails loud at parse (our fail-closed parser) - the bad port never ships.
    #[test]
    fn x8f7f_unknown_source_key_fails_loud() {
        let bad = "[[rule]]\nid = \"p\"\nstate = \"blocked\"\npriority = 1\n\
                   region = \"whole_recent\"\nvisible_blocker = true\n\
                   gate = { contains = \"x\" }\n";
        assert!(matches!(
            Manifest::parse(bad),
            Err(ManifestError::Field { .. })
        ));
    }

    // AC2-FR / AC3 (x-8f7f): the hosting gate is real. opencode's manifest is
    // BUNDLED (staged) but opencode has no provider impl, so it can never be
    // hosted as a pane and the manifest sits inert. agy, by contrast, is both
    // bundled AND hostable (has an AgyProvider), so its manifest can fire.
    #[test]
    fn x8f7f_staged_manifest_is_inert_without_a_host() {
        assert!(bundled_manifest("opencode").is_some(), "opencode staged");
        assert!(
            crate::provider::for_name("opencode").is_none(),
            "opencode is not hostable (no provider impl) -> manifest inert",
        );
        assert!(bundled_manifest("agy").is_some(), "agy staged");
        assert!(
            crate::provider::for_name("agy").is_some(),
            "agy IS hostable (AgyProvider) -> manifest can fire",
        );
    }
}

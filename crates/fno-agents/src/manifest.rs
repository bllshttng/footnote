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
//! Scope (design `2026-06-26-inside-out-detection-manifests.md`, the E6.2 leaf):
//! parser + region vocabulary + gate evaluator + priority arbiter. NOT in scope:
//! the actual `claude.toml`/`codex.toml`/`gemini.toml` rule files (E6.3) and
//! wiring the verdict into the daemon state badge (E6.3+E2), so nothing here is
//! `include_str!`'d or called from the runtime yet. Remote/cached/version-gated
//! resolution is a logged fast-follow (`min_engine_version` is parsed now so a
//! later remote manifest can gate, but is otherwise unused).
//!
//! ponytail: a rule's regexes recompile on each `evaluate` (regions are tiny,
//! evaluate runs at human-perception cadence on readiness polls); cache compiled
//! `Regex`es per rule if a profiler ever flags it. The `prompt_box_body` region
//! and `skip_state_update`/priority semantics are tuned against herdr's design,
//! not yet against a live claude TUI (E6.3's job).

use crate::readiness::ScreenView;
use regex::Regex;

/// Max nesting depth for a [`Gate`] tree. A pathological manifest (deeply nested
/// `all`/`any`/`not`) is refused at parse rather than risking a stack blow at
/// evaluate. 16 is far past any real rule (herdr's deepest is ~3).
const MAX_GATE_DEPTH: usize = 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest is not valid TOML: {0}")]
    Toml(String),
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
/// it does not handle nested boxes or ASCII `+--+` frames. Widen it in E6.3 if a
/// real capture draws something this misses - the failure mode is an empty region
/// (rule doesn't fire), never a wrong grid.
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
        let as_array = || {
            val.as_array().ok_or_else(|| ManifestError::Field {
                rule: rule.to_string(),
                field: format!("gate.{key}"),
            })
        };
        match key.as_str() {
            "contains" => Ok(Gate::Contains(as_str()?.to_string())),
            "regex" => Ok(Gate::Regex(compile(as_str()?)?)),
            "line_regex" => Ok(Gate::LineRegex(compile(as_str()?)?)),
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
        let str_field = |f: &str| {
            v.get(f)
                .and_then(|x| x.as_str())
                .ok_or_else(|| ManifestError::Field {
                    rule: id.clone(),
                    field: f.to_string(),
                })
        };
        let state = str_field("state")?.to_string();
        let priority = v
            .get("priority")
            .and_then(|x| x.as_integer())
            .ok_or_else(|| ManifestError::Field {
                rule: id.clone(),
                field: "priority".to_string(),
            })? as i32;
        let region = Region::parse(str_field("region")?, &id)?;
        let skip_state_update = v
            .get("skip_state_update")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let gate_val = v.get("gate").ok_or_else(|| ManifestError::Field {
            rule: id.clone(),
            field: "gate".to_string(),
        })?;
        let gate = Gate::parse(gate_val, &id, 0)?;
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
    pub rules: Vec<ManifestRule>,
}

impl Manifest {
    /// Parse a manifest TOML. Fails closed: a bad regex, unknown region, or
    /// over-deep gate is a parse error naming the offending rule, never a
    /// silently-dropped rule.
    pub fn parse(s: &str) -> Result<Manifest, ManifestError> {
        let root: toml::Value =
            toml::from_str(s).map_err(|e| ManifestError::Toml(e.to_string()))?;
        let min_engine_version = root
            .get("min_engine_version")
            .and_then(|v| v.as_integer())
            .unwrap_or(0)
            .max(0) as u32;
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
        assert_eq!(m.rules.len(), 2);
        assert_eq!(m.rules[0].id, "high", "highest priority sorts first");
        assert_eq!(m.rules[1].id, "low");
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
        assert!(m.rules.is_empty());
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
}

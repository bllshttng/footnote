//! A crown's term: the bound a reign declares at coronation so a healthy
//! board can no longer wait forever. Pure reading logic only; the Stop-hook
//! gate (`loopcheck::king_decide`) and the declaration verb
//! (`loop_reign::run_reign_term`) call into this module rather than
//! duplicating the spec parse or the state comparison.

use crate::loopcheck::KingManifest;
use chrono::{DateTime, Utc};
use std::path::Path;

/// eval: ideal handoff ~100h in, end of window 12
/// (`internal/fno/evals/kings/king-a792-control-...md` part 4, reform R2).
pub const DEFAULT_TERM: &str = "span:96h";

const LEGAL_FORMS: &str = "legal forms: span:<N>[smhd], compactions:<N>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermSpec {
    Span(u64),
    Compactions(u64),
}

impl TermSpec {
    pub fn is_compactions(&self) -> bool {
        matches!(self, TermSpec::Compactions(_))
    }
}

/// `span:<N>[smhd]` or `compactions:<N>`, N >= 1. Any other shape, including
/// a zero-length span, refuses naming both legal forms.
pub fn parse_spec(raw: &str) -> Result<TermSpec, String> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("span:") {
        return match parse_duration_secs(rest) {
            Some(secs) if secs > 0 => Ok(TermSpec::Span(secs)),
            _ => Err(format!("bad term spec {raw:?}; {LEGAL_FORMS}")),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("compactions:") {
        return match rest.parse::<u64>() {
            Ok(n) if n >= 1 => Ok(TermSpec::Compactions(n)),
            _ => Err(format!("bad term spec {raw:?}; {LEGAL_FORMS}")),
        };
    }
    Err(format!("bad term spec {raw:?}; {LEGAL_FORMS}"))
}

fn parse_duration_secs(rest: &str) -> Option<u64> {
    if rest.len() < 2 {
        return None;
    }
    let (num, unit) = rest.split_at(rest.len() - 1);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    Some(n * mult)
}

#[derive(Debug, Clone, PartialEq)]
pub enum TermState {
    Within { used: String, of: String },
    Reached { used: String, of: String },
    Unreadable(String),
}

#[derive(Debug, Clone)]
pub struct TermReading {
    /// The raw spec string in force: the manifest's own, or [`DEFAULT_TERM`].
    pub spec: String,
    /// Whether a king declared this spec, as opposed to it reading the default.
    pub declared: bool,
    pub state: TermState,
}

fn format_span(secs: u64) -> String {
    format!("{}h", secs / 3600)
}

/// Reads whether `m`'s crown has reached its term. `transcript`, when the
/// spec is a `compactions:` kind, is the caller's already-resolved transcript
/// path (`claude_drive::find_transcript_in`); a `None` there is always
/// `Unreadable`, never `Within` - an unmeasured reading is never a healthy one.
pub(crate) fn reading(
    m: &KingManifest,
    now: DateTime<Utc>,
    transcript: Option<&Path>,
) -> TermReading {
    let (raw_spec, declared) = match m.term.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => (s.to_string(), true),
        _ => (DEFAULT_TERM.to_string(), false),
    };
    let spec = match parse_spec(&raw_spec) {
        Ok(s) => s,
        Err(e) => {
            return TermReading {
                spec: raw_spec,
                declared,
                state: TermState::Unreadable(e),
            };
        }
    };
    let Some(created) = m
        .created_at
        .as_deref()
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| ts.with_timezone(&Utc))
    else {
        return TermReading {
            spec: raw_spec,
            declared,
            state: TermState::Unreadable("manifest carries no readable created_at".to_string()),
        };
    };
    match spec {
        TermSpec::Span(ceiling_secs) => {
            let used_secs = (now - created).num_seconds().max(0) as u64;
            let state = if used_secs >= ceiling_secs {
                TermState::Reached {
                    used: format_span(used_secs),
                    of: format_span(ceiling_secs),
                }
            } else {
                TermState::Within {
                    used: format_span(used_secs),
                    of: format_span(ceiling_secs),
                }
            };
            TermReading {
                spec: raw_spec,
                declared,
                state,
            }
        }
        TermSpec::Compactions(ceiling) => {
            let Some(path) = transcript else {
                return TermReading {
                    spec: raw_spec,
                    declared,
                    state: TermState::Unreadable(
                        "no transcript available for a compactions term".to_string(),
                    ),
                };
            };
            match crate::compaction::count_boundaries_since(path, Some(created.timestamp())) {
                Ok(count) => {
                    let state = if count >= ceiling {
                        TermState::Reached {
                            used: count.to_string(),
                            of: ceiling.to_string(),
                        }
                    } else {
                        TermState::Within {
                            used: count.to_string(),
                            of: ceiling.to_string(),
                        }
                    };
                    TermReading {
                        spec: raw_spec,
                        declared,
                        state,
                    }
                }
                Err(e) => TermReading {
                    spec: raw_spec,
                    declared,
                    state: TermState::Unreadable(format!("{}: {e}", path.display())),
                },
            }
        }
    }
}

/// The short word `king_loop_check`/verdict payloads carry for `state`.
pub fn state_word(state: &TermState) -> &'static str {
    match state {
        TermState::Within { .. } => "within",
        TermState::Reached { .. } => "reached",
        TermState::Unreadable(_) => "unreadable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(created_at: Option<&str>, term: Option<&str>) -> KingManifest {
        KingManifest {
            fno_id: "20260916T000000Z-kg1-abcdef".to_string(),
            scope: "x-test".to_string(),
            created_at: created_at.map(str::to_string),
            term: term.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn undeclared_default_reaches_after_96h() {
        let now = Utc::now();
        let created = now - chrono::Duration::hours(100);
        let m = manifest(Some(&created.to_rfc3339()), None);
        let r = reading(&m, now, None);
        assert!(!r.declared);
        assert_eq!(r.spec, DEFAULT_TERM);
        assert!(matches!(r.state, TermState::Reached { .. }));
    }

    #[test]
    fn compactions_term_with_no_transcript_is_unreadable_never_within() {
        let now = Utc::now();
        let m = manifest(Some(&now.to_rfc3339()), Some("compactions:12"));
        let r = reading(&m, now, None);
        assert!(matches!(r.state, TermState::Unreadable(_)));
    }

    #[test]
    fn parse_spec_rejects_zero_span_and_unknown_unit_naming_legal_forms() {
        for bad in ["span:0", "weeks:2", "compactions:0", "compactions:abc"] {
            let err = parse_spec(bad).unwrap_err();
            assert!(
                err.contains("legal forms"),
                "{bad} named legal forms: {err}"
            );
        }
    }

    #[test]
    fn parse_spec_accepts_span_and_compactions() {
        assert_eq!(parse_spec("span:96h").unwrap(), TermSpec::Span(96 * 3600));
        assert_eq!(parse_spec("span:30m").unwrap(), TermSpec::Span(30 * 60));
        assert_eq!(
            parse_spec("compactions:10").unwrap(),
            TermSpec::Compactions(10)
        );
    }

    #[test]
    fn within_term_reports_within() {
        let now = Utc::now();
        let created = now - chrono::Duration::hours(10);
        let m = manifest(Some(&created.to_rfc3339()), Some("span:72h"));
        let r = reading(&m, now, None);
        assert!(r.declared);
        assert!(matches!(r.state, TermState::Within { .. }));
    }
}

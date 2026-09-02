//! (x-1b35) Lane-color resolution for the sideline agent rows.
//!
//! The operator asked "which row belongs to which lane?" - and the honest
//! answer is the ROUTE (the vendor lane a row bills), not the harness alone:
//! zai and openrouter both drive harness=claude through
//! `ANTHROPIC_BASE_URL` + model overrides, so one harness color collapses
//! exactly the lanes that differ in spend. Model alone also fails (GLM via
//! zai and GLM via openrouter share a string on different bills).
//!
//! Color resolves through ONE fixed specificity cascade, most specific
//! first, first declaration found wins. There is deliberately no precedence
//! knob: naming the specific thing IS the intent, and a configurable
//! ranking would only let a config contradict its own declarations (CSS
//! resolves specificity classes, and does not let you reorder them).
//!
//! Config (read once per process, same file precedence as every other
//! `agents_view` settings read - project `.fno/config.toml`, then the
//! `$FNO_GLOBAL_SETTINGS_PATH` sibling, then `~/.fno/config.toml`):
//!
//! ```toml
//! [sideline.colors.harness]
//! codex = "cyan"
//! [sideline.colors.route]
//! openrouter = "magenta"
//! [sideline.colors.model]
//! "glm-5.3-flash[1m]" = "green"
//! [sideline.colors.row]
//! zai-glm-flash = "orange"          # a [[routing.models]] row name
//! [[routing.models]]
//! name = "zai-glm-flash"
//! color = "green"                   # the most specific key of all
//! ```
//!
//! Every key names its axis explicitly (`harness.` / `route.` / `model.` /
//! `row.`); a bare key under `[sideline.colors]` is refused at the config
//! layer because it is ambiguous between axes (AGENTS.md: never infer the
//! axis from a value). The Python config model enforces that refusal; this
//! reader simply never sees bare keys.

use std::path::PathBuf;

use crate::proto::Color;

/// One declared `[[routing.models]]` row, the fields the cascade matches on.
/// A mirror of the Python `RoutingModelBlock` (the crates share no types;
/// config.toml is the contract), read tolerantly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingRow {
    pub name: String,
    pub harness: String,
    pub model: String,
    pub route: String,
    pub account: String,
    pub color: String,
}

/// The parsed `[sideline.colors]` tables plus the routing inventory's colored
/// rows. All lookups are exact-string; an unknown color name is ignored at
/// resolve time (a config typo can never blank or mis-color a row).
#[derive(Debug, Clone, Default)]
pub struct SidelinePalette {
    pub harness: Vec<(String, String)>,
    pub route: Vec<(String, String)>,
    pub model: Vec<(String, String)>,
    /// routing-row NAME -> color (the `[sideline.colors.row]` table).
    pub row: Vec<(String, String)>,
    pub routing_rows: Vec<RoutingRow>,
}

impl SidelinePalette {
    fn table_color(tables: &[(String, String)], key: &str) -> Option<Color> {
        tables
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, c)| parse_color(c))
    }
}

/// Parse one configured color name. The ANSI-16 names cover the curated
/// defaults; `indexed(<n>)` and `#rrggbb` cover everything else an operator
/// may want. `None` = unrecognized, and resolution continues down the
/// cascade.
pub fn parse_color(name: &str) -> Option<Color> {
    let n = name.trim().to_ascii_lowercase();
    let ansi = match n.as_str() {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        "gray" | "grey" => 8,
        "light_red" => 9,
        "light_green" => 10,
        "light_yellow" => 11,
        "light_blue" => 12,
        "light_magenta" => 13,
        "light_cyan" => 14,
        "light_white" => 15,
        _ => return indexed_or_hex(&n),
    };
    Some(Color::Indexed(ansi))
}

fn indexed_or_hex(n: &str) -> Option<Color> {
    if let Some(rest) = n.strip_prefix("indexed(").and_then(|r| r.strip_suffix(')')) {
        return rest.trim().parse::<u8>().ok().map(Color::Indexed);
    }
    let hex = n.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Read the config file the same precedence every other agent-view settings
/// read uses (agents_view: project `.fno/config.toml`, then the
/// `$FNO_GLOBAL_SETTINGS_PATH` sibling, else `~/.fno/config.toml`). Returns
/// the first that parses; a missing or malformed file yields the empty
/// palette (zero-config rendering stays on the built-in table).
fn read_palette() -> SidelinePalette {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(pwd) = std::env::var_os("PWD")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
    {
        candidates.push(pwd.join(".fno").join("config.toml"));
    }
    let global = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).with_file_name("config.toml"))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno").join("config.toml"))
        });
    if let Some(g) = global {
        candidates.push(g);
    }
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        let mut pal = SidelinePalette::default();
        if let Some(colors) = doc.get("sideline").and_then(|s| s.get("colors")) {
            let pairs = |table: Option<&toml::Value>| -> Vec<(String, String)> {
                table
                    .and_then(|t| t.as_table())
                    .map(|t| {
                        t.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            pal.harness = pairs(colors.get("harness"));
            pal.route = pairs(colors.get("route"));
            pal.model = pairs(colors.get("model"));
            pal.row = pairs(colors.get("row"));
        }
        if let Some(rows) = doc
            .get("routing")
            .and_then(|r| r.get("models"))
            .and_then(|m| m.as_array())
        {
            pal.routing_rows = rows
                .iter()
                .filter_map(|r| r.as_table())
                .map(|t| RoutingRow {
                    name: t.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    harness: t
                        .get("harness")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    model: t.get("model").and_then(|v| v.as_str()).unwrap_or("").into(),
                    route: t.get("route").and_then(|v| v.as_str()).unwrap_or("").into(),
                    account: t
                        .get("account")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    color: t.get("color").and_then(|v| v.as_str()).unwrap_or("").into(),
                })
                .collect();
        }
        return pal;
    }
    SidelinePalette::default()
}

static PAL: std::sync::RwLock<Option<&'static SidelinePalette>> = std::sync::RwLock::new(None);

/// The currently cached palette, re-reading config on first call. The read
/// accessor the settings UI uses to list configured colors without
/// re-parsing config itself.
///
/// The cache holds a `&'static` into a leaked `Box`, so outstanding
/// references stay valid across a reload - `reload_palette` drops the CACHE
/// entry, never the memory a reader may still hold. A reload race can leak
/// one orphaned palette (small, bounded by reload count).
pub fn palette() -> &'static SidelinePalette {
    if let Some(p) = *PAL.read().unwrap() {
        return p;
    }
    let fresh: &'static SidelinePalette = Box::leak(Box::new(read_palette()));
    *PAL.write().unwrap() = Some(fresh);
    fresh
}

/// Invalidate the cached palette so the next [`palette`] re-reads config from
/// disk. Call after a successful `fno config set` so a saved color goes live
/// in the running mux without a restart. The cascade itself is untouched.
pub fn reload_palette() {
    *PAL.write().unwrap() = None;
}

/// The built-in fallback table, consulted only when nothing in config
/// declares a color for this row. Route-keyed first (the bill-separating
/// axis, the default thing that is colored), then harness-keyed. An axis
/// absent here renders `Color::Default` - silence, never a wrong lane.
fn builtin_color(axis: Axis, value: &str) -> Option<Color> {
    let named = |idx: u8| Some(Color::Indexed(idx));
    match axis {
        Axis::Route => match value {
            "zai" => named(2),        // green: the GLM-via-zai bill
            "openrouter" => named(5), // magenta: the open catalog bill
            "openai" => named(4),     // blue: codex / GPT lanes
            "anthropic" => named(6),  // cyan: subscription claude
            _ => None,
        },
        Axis::Harness => match value {
            "codex" => named(4),
            "agy" => named(3),
            "opencode" => named(13),
            "cursor" => named(12),
            "pi" => named(11),
            _ => None,
        },
    }
}

/// The two axes the built-in table speaks for; Row and Model are
/// config-only keys (no built-in can know an operator's model catalog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Route,
    Harness,
}

/// Resolve the lane color for one sideline row: the FIXED cascade, most
/// specific first, first declaration found wins.
///
/// 1. A matching `[[routing.models]]` row (every non-empty field must equal
///    the agent's axes; `account` participates only when the agent row
///    carries one) with a `color` field, or a `[sideline.colors.row]` entry
///    naming that row.
/// 2. `model."<model>"` - the exact recorded string.
/// 3. `route.<route>`.
/// 4. `harness.<harness>`.
/// 5. The built-in table (route first, then harness) - zero config still
///    renders curated lane colors.
pub fn resolve_lane_color(
    harness: Option<&str>,
    model: Option<&str>,
    route: Option<&str>,
    account: Option<&str>,
) -> Option<Color> {
    resolve_with(palette(), harness, model, route, account)
}

/// The cascade over an EXPLICIT palette - the testable seam behind
/// [`resolve_lane_color`] (the process palette is cached over real
/// config files, so the config-driven cascade levels need this injection
/// point for unit coverage).
pub fn resolve_with(
    pal: &SidelinePalette,
    harness: Option<&str>,
    model: Option<&str>,
    route: Option<&str>,
    account: Option<&str>,
) -> Option<Color> {
    // 1. The routing row: every declared (non-empty) field must match the
    // agent's axes (an empty declared field is a wildcard); the first
    // matching row carrying a color wins (first declaration wins).
    let matches =
        |declared: &str, actual: Option<&str>| declared.is_empty() || actual == Some(declared);
    let matched_row = pal.routing_rows.iter().find(|r| {
        (r.harness.is_empty() && r.model.is_empty() && r.route.is_empty() && r.account.is_empty())
            == false
            && matches(&r.harness, harness)
            && matches(&r.model, model)
            && matches(&r.route, route)
            && matches(&r.account, account)
    });
    if let Some(r) = matched_row {
        if !r.color.is_empty() {
            if let Some(c) = parse_color(&r.color) {
                return Some(c);
            }
        }
        if let Some(c) = SidelinePalette::table_color(&pal.row, &r.name) {
            return Some(c);
        }
    }
    if let Some(m) = model {
        if let Some(c) = SidelinePalette::table_color(&pal.model, m) {
            return Some(c);
        }
    }
    if let Some(rt) = route {
        if let Some(c) =
            SidelinePalette::table_color(&pal.route, rt).or_else(|| builtin_color(Axis::Route, rt))
        {
            return Some(c);
        }
    }
    if let Some(h) = harness {
        if let Some(c) = SidelinePalette::table_color(&pal.harness, h)
            .or_else(|| builtin_color(Axis::Harness, h))
        {
            return Some(c);
        }
    }
    None
}

/// The model-deviation token: the model's vendor prefix (the first `-`
/// segment) when the model is NOT in its harness's native family, `None`
/// otherwise. This is the textual accessibility channel for the lane color
/// (colorblind viewers, monochrome terminals) and the marker on rows that
/// are OFF their harness's default lane - claude on opus renders nothing;
/// claude on glm-5.3-flash renders ` glm`.
///
/// `ponytail:` the native-family map is a fixed render heuristic, not
/// config: the color channel is primary and configurable, this token is the
/// fallback, and a new vendor is one map line when one appears.
pub fn deviation_token(harness: Option<&str>, model: Option<&str>) -> Option<String> {
    let model = model?;
    let prefix = model.split('-').next()?.trim();
    if prefix.is_empty() {
        return None;
    }
    let native: &[&str] = match harness.unwrap_or("") {
        "claude" => &["claude", "opus", "sonnet", "haiku", "fable"],
        "codex" => &["gpt", "codex"],
        "agy" => &["gemini"],
        // A harness with no recorded family still renders its prefix: a
        // positive signal beats silence (the operator asked WHICH lane).
        _ => &[],
    };
    let lower = prefix.to_ascii_lowercase();
    if native.contains(&lower.as_str()) {
        None
    } else {
        Some(lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The palette tests run against the empty process palette; they pin the
    // CASCADE mechanics and the built-in table, never ambient config.

    #[test]
    fn builtin_table_colors_by_route_then_harness() {
        assert_eq!(
            resolve_lane_color(Some("claude"), None, Some("zai"), None),
            Some(Color::Indexed(2)),
            "route is the default colored axis"
        );
        assert_eq!(
            resolve_lane_color(Some("claude"), None, Some("anthropic"), None),
            Some(Color::Indexed(6))
        );
        assert_eq!(
            resolve_lane_color(Some("codex"), None, None, None),
            Some(Color::Indexed(4)),
            "harness fallback when no route recorded"
        );
        assert_eq!(
            resolve_lane_color(Some("claude"), None, None, None),
            None,
            "claude carries no builtin: the route axis decides"
        );
        assert_eq!(resolve_lane_color(None, None, None, None), None);
    }

    #[test]
    fn unknown_color_names_are_ignored() {
        assert_eq!(parse_color("chartreuse"), None);
        assert_eq!(parse_color("cyan"), Some(Color::Indexed(6)));
        assert_eq!(parse_color("indexed(200)"), Some(Color::Indexed(200)));
        assert_eq!(parse_color("#12abF0"), Some(Color::Rgb(0x12, 0xab, 0xf0)));
        // An unknown color inside a cascade position falls THROUGH to the
        // built-in rather than blanking the row.
        assert_eq!(
            resolve_lane_color(Some("claude"), None, Some("zai"), None),
            Some(Color::Indexed(2))
        );
    }

    #[test]
    fn deviation_token_marks_only_off_default_models() {
        assert_eq!(deviation_token(Some("claude"), Some("opus")), None);
        assert_eq!(deviation_token(Some("claude"), Some("opus-5")), None);
        assert_eq!(deviation_token(Some("claude"), Some("claude-opus-5")), None);
        assert_eq!(
            deviation_token(Some("claude"), Some("glm-5.3-flash[1m]")),
            Some("glm".to_string())
        );
        assert_eq!(deviation_token(Some("codex"), Some("gpt-5.6-luna")), None);
        assert_eq!(
            deviation_token(None, Some("glm-5.3-flash[1m]")),
            Some("glm".to_string()),
            "a row with no harness still renders a positive signal"
        );
        assert_eq!(deviation_token(Some("claude"), None), None);
    }

    #[test]
    fn parse_color_rejects_malformed_hex_and_indexed() {
        assert_eq!(parse_color("#12a"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
        assert_eq!(parse_color("indexed("), None);
        assert_eq!(
            parse_color("indexed(999)"),
            None,
            "u8 range is the contract"
        );
    }

    fn pal(
        rows: Vec<RoutingRow>,
        model: &str,
        route: &str,
        harness: &str,
        row: &str,
    ) -> SidelinePalette {
        let pairs = |s: &str| -> Vec<(String, String)> {
            s.split(',')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap();
                    (k.to_string(), v.to_string())
                })
                .collect()
        };
        SidelinePalette {
            routing_rows: rows,
            model: pairs(model),
            route: pairs(route),
            harness: pairs(harness),
            row: pairs(row),
        }
    }

    #[test]
    fn the_config_cascade_levels_fire_in_specificity_order() {
        // Level 1: a routing row matching on harness+model wins over every
        // coarser table.
        let p = pal(
            vec![RoutingRow {
                name: "zai-glm".into(),
                harness: "claude".into(),
                model: "glm-5.3-flash[1m]".into(),
                color: "green".into(),
                ..Default::default()
            }],
            "glm-5.3-flash[1m]=yellow",
            "zai=blue",
            "claude=cyan",
            "",
        );
        assert_eq!(
            resolve_with(
                &p,
                Some("claude"),
                Some("glm-5.3-flash[1m]"),
                Some("zai"),
                None
            ),
            Some(Color::Indexed(2)),
            "routing row outranks model/route/harness tables"
        );
        // Level 2: with no routing row, the model table wins.
        let p2 = pal(
            vec![],
            "glm-5.3-flash[1m]=yellow",
            "zai=blue",
            "claude=cyan",
            "",
        );
        assert_eq!(
            resolve_with(
                &p2,
                Some("claude"),
                Some("glm-5.3-flash[1m]"),
                Some("zai"),
                None
            ),
            Some(Color::Indexed(3)),
            "model table beats route and harness"
        );
        // Level 3: route table; level 4: harness table.
        let p3 = pal(vec![], "", "zai=blue", "claude=cyan", "");
        assert_eq!(
            resolve_with(
                &p3,
                Some("claude"),
                Some("glm-5.3-flash[1m]"),
                Some("zai"),
                None
            ),
            Some(Color::Indexed(4))
        );
        assert_eq!(
            resolve_with(&p3, Some("claude"), None, None, None),
            Some(Color::Indexed(6))
        );
    }

    #[test]
    fn routing_row_wildcards_match_and_account_participates_only_when_carried() {
        // A row declaring only route matches any harness/model on that route.
        let p = pal(
            vec![RoutingRow {
                name: "or".into(),
                route: "openrouter".into(),
                color: "magenta".into(),
                ..Default::default()
            }],
            "",
            "",
            "",
            "",
        );
        assert_eq!(
            resolve_with(
                &p,
                Some("codex"),
                Some("deepseek-r1"),
                Some("openrouter"),
                None
            ),
            Some(Color::Indexed(5))
        );
        // An account-declaring row does NOT match a row carrying no account.
        let pa = pal(
            vec![RoutingRow {
                name: "mk".into(),
                account: "makers".into(),
                color: "red".into(),
                ..Default::default()
            }],
            "",
            "",
            "",
            "",
        );
        assert_eq!(
            resolve_with(&pa, Some("claude"), None, None, None),
            None,
            "declared account vs no account: no match"
        );
        assert_eq!(
            resolve_with(&pa, Some("claude"), None, None, Some("makers")),
            Some(Color::Indexed(1))
        );
    }

    #[test]
    fn first_declaration_wins_and_unknown_colors_fall_through() {
        // Two matching rows: the FIRST declared wins even when the second is
        // more specific.
        let p = pal(
            vec![
                RoutingRow {
                    name: "broad".into(),
                    route: "zai".into(),
                    color: "red".into(),
                    ..Default::default()
                },
                RoutingRow {
                    name: "narrow".into(),
                    harness: "claude".into(),
                    route: "zai".into(),
                    color: "blue".into(),
                    ..Default::default()
                },
            ],
            "",
            "",
            "",
            "",
        );
        assert_eq!(
            resolve_with(&p, Some("claude"), None, Some("zai"), None),
            Some(Color::Indexed(1)),
            "first declaration wins"
        );
        // An unknown color on the winning row falls through to the NEXT
        // cascade level, never blanks the row.
        let p2 = pal(
            vec![RoutingRow {
                name: "typo".into(),
                route: "zai".into(),
                color: "chartreuse".into(),
                ..Default::default()
            }],
            "",
            "zai=green",
            "",
            "",
        );
        assert_eq!(
            resolve_with(&p2, Some("claude"), None, Some("zai"), None),
            Some(Color::Indexed(2)),
            "unknown color falls through to the route table"
        );
        // A row whose row-table entry names it also resolves (the row.color
        // field empty path).
        let p3 = pal(
            vec![RoutingRow {
                name: "named".into(),
                route: "zai".into(),
                ..Default::default()
            }],
            "",
            "",
            "",
            "named=yellow",
        );
        assert_eq!(
            resolve_with(&p3, None, None, Some("zai"), None),
            Some(Color::Indexed(3)),
            "the row table names the matched row"
        );
    }

    // (x-e4f1) The settings UI reads the cached palette through the pub
    // accessor and invalidates it through reload_palette; both behave on the
    // module-scope cache.
    #[test]
    fn palette_accessor_exposes_all_four_axes() {
        let p = pal(
            vec![],
            "glm-5.3-flash[1m]=yellow",
            "zai=green",
            "codex=cyan",
            "named=red",
        );
        assert_eq!(p.harness, vec![("codex".into(), "cyan".into())]);
        assert_eq!(p.route, vec![("zai".into(), "green".into())]);
        assert_eq!(p.model, vec![("glm-5.3-flash[1m]".into(), "yellow".into())]);
        assert_eq!(p.row, vec![("named".into(), "red".into())]);
        // The accessor hands out one stable cached instance between reloads.
        assert!(
            std::ptr::eq(palette(), palette()),
            "palette() returns the same cached instance between reloads"
        );
    }

    #[test]
    fn reload_palette_invalidates_so_the_next_read_reinitializes() {
        // prime, invalidate, re-prime: the observable contract the settings
        // save path depends on (write config, reload, resolve sees it).
        let _ = palette();
        assert!(PAL.read().unwrap().is_some(), "palette() primes the cache");
        reload_palette();
        assert!(
            PAL.read().unwrap().is_none(),
            "reload_palette clears the cache"
        );
        let _ = palette();
        assert!(
            PAL.read().unwrap().is_some(),
            "the next palette() re-primes"
        );
    }
}

//! `reign-ledger`: the Crown Ledger page for `fno agents king ledger`.
//!
//! Python resolves the court (registry adjudication, manifest limbs, the
//! caller's paths) and hands one court JSON over; the native side owns the
//! page assembly, the same split `king-history` applies to the journal
//! readback, so the Python-tree ratchet holds. The crown-to-nodes join stays
//! in the fold the Python side already ran (`scope_nodes` rides in the court
//! JSON); titles, uncrowned epics, and orphan leaves are read from the graph
//! through the SAME compiler `court-fold` uses, so the page cannot disagree
//! with the court about who holds a node.
//!
//! `reign-ledger --court-json PATH --graph PATH --generated TS --out PATH`
//!
//! rc 0 wrote the page, 1 render or read failure, 2 usage failure.
use crate::court_fold::{compile_forced, esc, ACTIVE_STATUSES, COUNT_ORDER};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn as_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn plural(n: i64, one: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        format!("{one}s")
    }
}

fn status_label(status: &str) -> String {
    status.replace('_', " ")
}

/// The member ids of one crown at its own level, or None when the scope
/// cannot compile (the fold's verdict already says so in place).
fn members_of(
    crown: &Value,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> Option<BTreeSet<String>> {
    let scope = s_str(crown, "scope")?;
    let level = crown.get("level").and_then(|l| l.as_i64())?;
    compile_forced(scope, entries, projects, level).ok()
}

fn titles_of(entries: &[Value]) -> BTreeMap<String, &Value> {
    let mut out = BTreeMap::new();
    for e in entries {
        if let Some(id) = s_str(e, "id") {
            out.insert(id.to_string(), e);
        }
    }
    out
}

/// Counts in lifecycle order, zero entries dropped; a status outside the
/// vocabulary keeps its place at the end rather than vanishing.
fn counts_in_order(fold: &Value) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    if let Some(counts) = fold.get("counts").and_then(|c| c.as_object()) {
        for status in COUNT_ORDER {
            if let Some(n) = counts.get(status).and_then(|v| v.as_i64()) {
                if n > 0 {
                    out.push((status.to_string(), n));
                }
            }
        }
        let mut leftovers: Vec<String> = counts
            .keys()
            .filter(|k| !COUNT_ORDER.contains(&k.as_str()))
            .cloned()
            .collect();
        leftovers.sort();
        for key in leftovers {
            let n = counts.get(&key).and_then(|v| v.as_i64()).unwrap_or(0);
            if n > 0 {
                out.push((key, n));
            }
        }
    }
    out
}

fn active_sum(fold: &Value) -> i64 {
    ACTIVE_STATUSES
        .iter()
        .filter_map(|s| {
            fold.get("counts")
                .and_then(|c| c.get(*s))
                .and_then(|v| v.as_i64())
        })
        .sum()
}

fn chip_td(status: &str) -> String {
    let s = esc(status);
    format!(
        "<td><span class=\"chip s-{s}\">{}</span></td>",
        esc(&status_label(status))
    )
}

/// The work cell names the slug in words; the graph entry's title rides in
/// the title attribute. A slug-less row falls back to the title itself.
fn work_cell(slug: &str, title: &str) -> String {
    if slug.is_empty() {
        let work = if title.is_empty() { "-" } else { title };
        return format!("<td class=\"slug\">{}</td>", esc(work));
    }
    let words = slug.replace('-', " ");
    if title.is_empty() {
        format!("<td class=\"slug\">{}</td>", esc(&words))
    } else {
        format!(
            "<td class=\"slug\" title=\"{}\">{}</td>",
            esc(title),
            esc(&words)
        )
    }
}

fn sess_cell(count: usize, worker: Option<&str>) -> String {
    let held = match worker {
        Some(w) if !w.is_empty() => format!(" · held by {}", esc(w)),
        _ => String::new(),
    };
    if count == 0 {
        format!("<td class=\"num\"><span class=\"sess zero\" title=\"no session recorded{held}\">0</span></td>")
    } else {
        format!("<td class=\"num\"><span class=\"sess\" title=\"{count} session(s) recorded{held}\">{count}</span></td>")
    }
}

/// The PR cell links only an https url; any other scheme (a javascript: url,
/// say) renders as plain text, and no PR at all renders a dash.
fn pr_cell(pr_number: Option<i64>, pr_url: Option<&str>) -> String {
    match pr_number {
        None => "<td class=\"num\"><span class=\"pr none\">-</span></td>".to_string(),
        Some(n) => {
            if pr_url.map(|u| u.starts_with("https://")).unwrap_or(false) {
                format!(
                    "<td class=\"num\"><a class=\"pr\" href=\"{}\">#{n}</a></td>",
                    esc(pr_url.unwrap())
                )
            } else {
                format!("<td class=\"num\">#{n}</td>")
            }
        }
    }
}

/// One row of the Active territory table; the fold row carries the live
/// fold data, the graph entry carries the title and pr_url.
fn active_row(n: &Value, titles: &BTreeMap<String, &Value>) -> String {
    let id = s_str(n, "id").unwrap_or("");
    let entry = titles.get(id).copied();
    let title = entry.and_then(|e| s_str(e, "title")).unwrap_or("");
    let slug = s_str(n, "slug").unwrap_or("");
    let pr_number = n.get("pr_number").and_then(|p| p.as_i64());
    let pr_url = entry.and_then(|e| s_str(e, "pr_url"));
    let worker = n.get("worker").and_then(|w| w.as_str());
    let sessions = n
        .get("sessions")
        .and_then(|s| s.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    format!(
        "<tr><td class=\"id\">{}</td>{}{}{}{}</tr>",
        esc(id),
        chip_td(s_str(n, "status").unwrap_or("")),
        work_cell(slug, title),
        sess_cell(sessions, worker),
        pr_cell(pr_number, pr_url),
    )
}

/// Rows for the uncrowned-epic and orphan-leaf tables, which read graph
/// entries directly rather than fold rows.
fn entry_row(e: &Value) -> String {
    format!(
        "<tr><td class=\"id\">{}</td><td class=\"slug\">{}</td>{}<td class=\"num\">{}</td></tr>",
        esc(s_str(e, "id").unwrap_or("")),
        esc(s_str(e, "title").unwrap_or("-")),
        chip_td(s_str(e, "status").unwrap_or("")),
        esc(s_str(e, "priority").unwrap_or("")),
    )
}

fn bar_and_legend(fold: &Value) -> String {
    let ordered = counts_in_order(fold);
    if ordered.is_empty() {
        return String::new();
    }
    let aria = ordered
        .iter()
        .map(|(s, n)| format!("{} {n}", status_label(s)))
        .collect::<Vec<_>>()
        .join(", ");
    let segs: String = ordered
        .iter()
        .map(|(s, n)| {
            let cls = esc(s);
            format!(
                "<span class=\"seg s-{cls}\" style=\"flex:{n}\" title=\"{}: {n}\"></span>",
                esc(&status_label(s))
            )
        })
        .collect();
    let legend: String = ordered
        .iter()
        .map(|(s, n)| {
            let cls = esc(s);
            format!(
                "<li><span class=\"dot s-{cls}\"></span><span class=\"lg-n\">{n}</span><span class=\"lg-l\">{}</span></li>",
                esc(&status_label(s))
            )
        })
        .collect();
    format!("<div class=\"bar\" role=\"img\" aria-label=\"{aria}\">{segs}</div><ul class=\"legend\">{legend}</ul>")
}

fn crown_card(crown: &Value, titles: &BTreeMap<String, &Value>) -> String {
    let level = crown.get("level").and_then(|l| l.as_i64());
    let rung = level
        .map(|l| format!("L{l}"))
        .unwrap_or_else(|| "L?".to_string());
    let card_cls = if level == Some(1) {
        "crown root"
    } else {
        "crown"
    };
    let status = s_str(crown, "status").unwrap_or("-");
    let dot_cls = if status == "live" {
        "status-dot live"
    } else {
        "status-dot other"
    };
    let (agree_txt, agree_cls) = match crown.get("agree") {
        Some(Value::Bool(true)) => ("agree", "tag tag-ok"),
        Some(Value::Bool(false)) => ("disagree", "tag tag-bad"),
        _ => ("unknown", "tag tag-warn"),
    };
    let source_tag = s_str(crown, "crown_source")
        .map(|src| {
            let cls = match src {
                "split" => "tag tag-bad",
                "row" | "manifest" => "tag tag-warn",
                _ => "tag",
            };
            format!(
                "<span class=\"{cls}\" title=\"which readers see this crown\">{}</span>",
                esc(src)
            )
        })
        .unwrap_or_default();
    let mut out = format!(
        "<article class=\"{card_cls}\"><header class=\"ch\"><div class=\"ch-l\">\
         <span class=\"rung\">{rung}</span><h2>{}</h2>\
         <span class=\"{dot_cls}\" title=\"crown status: {}\"></span></div>\
         <div class=\"ch-r\"><span class=\"{agree_cls}\">{agree_txt}</span>{source_tag}</div></header>",
        esc(s_str(crown, "scope").unwrap_or("-")),
        esc(status),
    );
    out.push_str(&format!(
        "<p class=\"holder\">held by <b>{}</b> · granted by {}</p>",
        esc(s_str(crown, "holder").unwrap_or("-")),
        esc(s_str(crown, "grantor").unwrap_or("-")),
    ));
    if let Some(reason) = s_str(crown, "reason") {
        out.push_str(&format!("<p class=\"holder\">{}</p>", esc(reason)));
    }
    let fold = crown.get("scope_nodes").cloned().unwrap_or(json!({}));
    if s_str(&fold, "status") == Some("unresolved") {
        out.push_str(&format!(
            "<p class=\"holder\">scope fold: unresolved - {}</p>",
            esc(s_str(&fold, "reason").unwrap_or(""))
        ));
    } else {
        let done = fold
            .get("counts")
            .and_then(|c| c.get("done"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        out.push_str(&format!(
            "<div class=\"stats\"><span><b>{}</b> nodes</span><span><b>{}</b> active</span><span><b>{done}</b> done</span></div>",
            as_i64(&fold, "total"),
            active_sum(&fold),
        ));
        out.push_str(&bar_and_legend(&fold));
        let rows: String = fold
            .get("nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|n| active_row(n, titles))
            .collect();
        out.push_str(&format!(
            "<div class=\"tscroll\"><table><caption>Active territory</caption>\
             <thead><tr><th>node</th><th>state</th><th>work</th><th class=\"num\">ses</th><th class=\"num\">PR</th></tr></thead>\
             <tbody>{rows}</tbody></table></div>"
        ));
    }
    out.push_str("</article>");
    out
}

fn rung_heading(level: Option<i64>, count: usize) -> String {
    match level {
        Some(1) => "Rung 1 - the whole project".to_string(),
        Some(n) => {
            let word = if count == 1 {
                "territory"
            } else {
                "territories"
            };
            format!("Rung {n} - {count} {word} granted beneath it")
        }
        None => "Rung ? - crowns with no level".to_string(),
    }
}

fn reader_str(v: Option<&Value>) -> &'static str {
    match v {
        Some(Value::Bool(true)) => "true",
        Some(Value::Bool(false)) => "false",
        _ => "unknown",
    }
}

/// A verdict tile; a count the court did not report renders ? and never 0.
fn fact(v: Option<&Value>, label: &str, bad: bool) -> String {
    let n = v.and_then(|x| x.as_i64());
    let shown = n.map_or("?".to_string(), |x| x.to_string());
    let cls = if bad { "fact bad" } else { "fact" };
    format!("<div class=\"{cls}\"><span class=\"fv\">{shown}</span><span class=\"fl\">{label}</span></div>")
}

/// The tile for a reading that could not run at all: a dash, never 0 and
/// never ?, because nothing was measured rather than a count the reader
/// dropped. Distinct from [`fact`]'s "?" on purpose: a missing summary key
/// and a read that failed are different facts.
fn fact_unmeasured(label: &str) -> String {
    format!(
        "<div class=\"fact\"><span class=\"fv\">-</span><span class=\"fl\">{label}</span></div>"
    )
}

/// Four states, never a falsely healthy page: the court cannot be read, it
/// holds no crowns, it agrees with itself, or it disagrees with itself. The
/// registry split read arrives precomputed so the card stays pure and a
/// test can pass a fabricated reading.
fn verdict_card(
    court: &Value,
    summary: &Value,
    split_read: &Result<crate::crown_split::CrownSplits, String>,
) -> String {
    let crowns = court.get("crowns").and_then(|c| c.as_array());
    let (cls, body) = match crowns {
        None => (
            "verdict bad",
            format!(
                "<div class=\"vmain\"><span class=\"mark\">!</span><span>The court cannot be read.</span></div>\
                 <p class=\"vnote\">{}</p>\
                 <p class=\"vnote\">This is not an empty court; nothing was checked.</p>",
                esc(s_str(summary, "reason").unwrap_or("reason unavailable"))
            ),
        ),
        Some(c) if c.is_empty() => (
            "verdict empty",
            "<div class=\"vmain\"><span>The court holds no live crowns.</span></div>".to_string(),
        ),
        Some(_) => {
            let disagreements = as_i64(summary, "disagreements");
            let unknowns = as_i64(summary, "unknowns");
            let splits = as_i64(summary, "splits");
            let (double_ruled, split_tiles, split_note) = match split_read {
                Ok(cs) => {
                    let d = cs.double_ruled.len() as i64;
                    let s = cs.stale.len() as i64;
                    (
                        Some(d),
                        format!(
                            "{}{}",
                            fact(Some(&json!(d)), "double ruled", d > 0),
                            fact(Some(&json!(s)), "stale crowns", s > 0),
                        ),
                        String::new(),
                    )
                }
                Err(reason) => (
                    None,
                    format!(
                        "{}{}",
                        fact_unmeasured("double ruled"),
                        fact_unmeasured("stale crowns")
                    ),
                    format!(
                        "<p class=\"vnote\">crown split read failed: {}</p>",
                        esc(reason)
                    ),
                ),
            };
            let (cls, mark, text) = if disagreements == 0
                && unknowns == 0
                && splits == 0
                && double_ruled.unwrap_or(0) == 0
            {
                ("verdict", "✓", "The court agrees with itself.")
            } else {
                ("verdict bad", "!", "The court disagrees with itself.")
            };
            let tiles = format!(
                "{}{}{}{}{}{}",
                fact(summary.get("total"), "crowns", false),
                fact(summary.get("splits"), "splits", splits > 0),
                fact(summary.get("disagreements"), "disagreements", disagreements > 0),
                fact(summary.get("unknowns"), "unknowns", unknowns > 0),
                fact(
                    summary.get("manifest_only"),
                    "manifest only",
                    as_i64(summary, "manifest_only") > 0
                ),
                split_tiles,
            );
            (
                cls,
                format!(
                    "<div class=\"vmain\"><span class=\"mark\">{mark}</span><span>{text}</span></div>\
                     <div class=\"facts\">{tiles}</div>{split_note}"
                ),
            )
        }
    };
    let mut sweep_note = String::new();
    if crowns.map(|c| !c.is_empty()).unwrap_or(false)
        && summary.get("sweep_ran") == Some(&Value::Bool(false))
    {
        sweep_note = "<p class=\"vnote\">orphan sweep did not run (stale or missing binary): zero manifest-only entries is an absence, not a finding</p>".to_string();
    }
    format!(
        "<section class=\"{cls}\">{body}{sweep_note}\
         <div class=\"readers\">graph_readable {} · registry_readable {} · sweep_ran {}</div></section>",
        reader_str(court.get("graph_readable")),
        reader_str(court.get("registry_readable")),
        reader_str(summary.get("sweep_ran")),
    )
}

/// Epics in no crown's territory: absent from every fold, so the page names
/// them instead of letting their absence read as zero.
fn uncrowned_section(compiled: &[Option<BTreeSet<String>>], entries: &[Value]) -> String {
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for members in compiled.iter().flatten() {
        covered.extend(members.iter().cloned());
    }
    let mut orphans: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            s_str(e, "type") == Some("epic") && !covered.contains(s_str(e, "id").unwrap_or(""))
        })
        .collect();
    if orphans.is_empty() {
        return String::new();
    }
    let p1 = orphans
        .iter()
        .filter(|e| s_str(e, "priority") == Some("p1"))
        .count();
    orphans.sort_by_key(|e| {
        (
            s_str(e, "priority").unwrap_or("p2").to_string(),
            s_str(e, "title").unwrap_or("").to_string(),
        )
    });
    let rows: String = orphans.iter().map(|e| entry_row(e)).collect();
    format!(
        "<h2 class=\"sect\">uncrowned epics</h2><article class=\"crown\">\
         <p class=\"holder\">{} uncrowned, {p1} at p1</p>\
         <div class=\"tscroll\"><table><caption>Uncrowned epics</caption>\
         <thead><tr><th>node</th><th>work</th><th>state</th><th class=\"num\">priority</th></tr></thead>\
         <tbody>{rows}</tbody></table></div></article>",
        orphans.len()
    )
}

/// Orphan leaves: parent falsy, status actionable, and the id is no node's
/// parent - work nobody contains and nobody contains the container of.
fn orphan_leaves_section(entries: &[Value]) -> String {
    let parents: BTreeSet<&str> = entries.iter().filter_map(|e| s_str(e, "parent")).collect();
    let mut leaves: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            // parent falsy: absent, null, or an empty string left by a
            // hand edit or a migration.
            matches!(s_str(e, "parent"), None | Some(""))
                && ACTIVE_STATUSES.contains(&s_str(e, "status").unwrap_or(""))
                && !parents.contains(s_str(e, "id").unwrap_or(""))
        })
        .collect();
    if leaves.is_empty() {
        return String::new();
    }
    let p1 = leaves
        .iter()
        .filter(|e| s_str(e, "priority") == Some("p1"))
        .count();
    leaves.sort_by_key(|e| {
        (
            s_str(e, "priority").unwrap_or("p2").to_string(),
            s_str(e, "title").unwrap_or("").to_string(),
        )
    });
    let rows: String = leaves.iter().map(|e| entry_row(e)).collect();
    let word = if leaves.len() == 1 {
        "orphan leaf"
    } else {
        "orphan leaves"
    };
    format!(
        "<h2 class=\"sect\">orphan leaves</h2><article class=\"crown\">\
         <p class=\"holder\">{n} {word}, {p1} at p1</p>\
         <div class=\"tscroll\"><table><caption>Orphan leaves</caption>\
         <thead><tr><th>node</th><th>work</th><th>state</th><th class=\"num\">priority</th></tr></thead>\
         <tbody>{rows}</tbody></table></div></article>",
        n = leaves.len(),
    )
}

// Palette and layout ported from the operator's approved Crown Ledger
// design; the `.tscroll{overflow-x:auto}` and `overflow-wrap:anywhere`
// guarantees from the phone-readability pass are kept verbatim.
const CSS: &str = r##":root{
  --ground:#F4F6F4; --surface:#FCFDFC; --surface-2:#EDF0EE;
  --ink:#191E1B; --ink-mut:#5B6661; --rule:#D7DCD8; --rule-2:#C3CAC5;
  --brass:#8A6A2F; --brass-soft:#B99A5E;
  --c-done:#3F7A52; --c-in_review:#3A6B8F; --c-in_progress:#A87318; --c-ready:#2E7D74;
  --c-design:#6B5896; --c-blocked:#A4483C; --c-idea:#93A09A; --c-deferred:#BAC3BE; --c-superseded:#D2D8D4;
  --ok:#3F7A52; --bad:#A4483C;
  --shadow:0 1px 2px rgba(25,30,27,.05),0 8px 24px -14px rgba(25,30,27,.22);
  --mono:"JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,monospace;
  --body:"Public Sans",system-ui,-apple-system,Segoe UI,sans-serif;
  --disp:"Fraunces","Iowan Old Style",Georgia,serif;
}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){
  --ground:#14181A; --surface:#1B2023; --surface-2:#232A2D;
  --ink:#E3E8E4; --ink-mut:#8B9993; --rule:#2B3337; --rule-2:#3A4448;
  --brass:#C99B45; --brass-soft:#8C6E31;
  --c-done:#6DBF87; --c-in_review:#7FB4DC; --c-in_progress:#DCAB4A; --c-ready:#5FBDB1;
  --c-design:#A996D6; --c-blocked:#E08878; --c-idea:#6B7873; --c-deferred:#404B47; --c-superseded:#2F3835;
  --ok:#6DBF87; --bad:#E08878;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px -14px rgba(0,0,0,.7);
}}
:root[data-theme="dark"]{
  --ground:#14181A; --surface:#1B2023; --surface-2:#232A2D;
  --ink:#E3E8E4; --ink-mut:#8B9993; --rule:#2B3337; --rule-2:#3A4448;
  --brass:#C99B45; --brass-soft:#8C6E31;
  --c-done:#6DBF87; --c-in_review:#7FB4DC; --c-in_progress:#DCAB4A; --c-ready:#5FBDB1;
  --c-design:#A996D6; --c-blocked:#E08878; --c-idea:#6B7873; --c-deferred:#404B47; --c-superseded:#2F3835;
  --ok:#6DBF87; --bad:#E08878;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px -14px rgba(0,0,0,.7);
}
*{box-sizing:border-box}
body{background:var(--ground);color:var(--ink);font-family:var(--body);line-height:1.5;
  -webkit-font-smoothing:antialiased;padding:clamp(20px,4vw,52px) clamp(16px,4vw,40px) 72px}
.wrap{max-width:1120px;margin:0 auto;display:flex;flex-direction:column;gap:34px}
a{color:var(--brass)}
:focus-visible{outline:2px solid var(--brass);outline-offset:2px;border-radius:3px}
.masthead{display:flex;flex-direction:column;gap:10px;border-bottom:2px solid var(--ink);padding-bottom:18px}
.eyebrow{font-family:var(--mono);font-size:11px;letter-spacing:.14em;text-transform:uppercase;color:var(--ink-mut)}
h1{font-family:var(--disp);font-weight:700;font-size:clamp(34px,5.6vw,58px);line-height:1.02;margin:0;
   letter-spacing:-.015em;text-wrap:balance}
.dek{margin:0;max-width:64ch;color:var(--ink-mut);font-size:15.5px}
.verdict{display:flex;flex-wrap:wrap;align-items:center;gap:18px 26px;padding:16px 20px;
  background:var(--surface);border:1px solid var(--rule);border-left:4px solid var(--ok);
  border-radius:2px;box-shadow:var(--shadow)}
.verdict.bad{border-left-color:var(--bad)}
.verdict.bad .mark{color:var(--bad)}
.verdict.empty{border-left-color:var(--rule-2)}
.vmain{display:flex;align-items:baseline;gap:10px;font-family:var(--disp);font-size:19px;font-weight:500}
.vmain .mark{color:var(--ok);font-family:var(--mono);font-weight:600}
.facts{display:flex;gap:22px;margin-left:auto;flex-wrap:wrap}
.fact{display:flex;flex-direction:column;align-items:flex-start}
.fv{font-family:var(--mono);font-size:19px;font-weight:600;font-variant-numeric:tabular-nums}
.fact.bad .fv{color:var(--bad)}
.fl{font-size:10.5px;letter-spacing:.1em;text-transform:uppercase;color:var(--ink-mut)}
.readers{font-family:var(--mono);font-size:11.5px;color:var(--ink-mut);width:100%;
  border-top:1px dashed var(--rule);padding-top:10px}
.vnote{margin:0;width:100%;font-size:12.5px;color:var(--ink-mut)}
.sect{font-family:var(--mono);font-size:11px;letter-spacing:.16em;text-transform:uppercase;
  color:var(--ink-mut);display:flex;align-items:center;gap:12px;margin:0}
.sect::after{content:"";flex:1;height:1px;background:var(--rule)}
.crown{background:var(--surface);border:1px solid var(--rule);border-radius:2px;padding:20px 22px 6px;
  display:flex;flex-direction:column;gap:13px;box-shadow:var(--shadow)}
.crown.root{border-top:3px solid var(--brass)}
.ch{display:flex;align-items:center;justify-content:space-between;gap:14px;flex-wrap:wrap}
.ch-l{display:flex;align-items:center;gap:11px}
.ch h2{font-family:var(--mono);font-size:20px;font-weight:600;margin:0;letter-spacing:-.01em}
.rung{font-family:var(--mono);font-size:10.5px;font-weight:600;letter-spacing:.08em;color:var(--brass);
  border:1px solid var(--brass-soft);border-radius:2px;padding:2px 6px}
.status-dot{width:8px;height:8px;border-radius:50%;background:var(--ok)}
.status-dot.other{background:var(--c-in_progress)}
.ch-r{display:flex;gap:7px}
.tag{font-family:var(--mono);font-size:10.5px;letter-spacing:.06em;padding:3px 8px;border-radius:2px;
  background:var(--surface-2);color:var(--ink-mut);border:1px solid var(--rule)}
.tag-ok{color:var(--ok);border-color:color-mix(in srgb,var(--ok) 40%,transparent)}
.tag-bad{color:var(--bad);border-color:var(--bad)}
.tag-warn{color:var(--c-in_progress);border-color:color-mix(in srgb,var(--c-in_progress) 45%,transparent)}
.holder{margin:0;font-size:13.5px;color:var(--ink-mut)}
.holder b{color:var(--ink);font-family:var(--mono);font-weight:600;font-size:13px}
.stats{display:flex;gap:20px;font-size:12.5px;color:var(--ink-mut)}
.stats b{font-family:var(--mono);font-size:15px;color:var(--ink);font-variant-numeric:tabular-nums}
.bar{display:flex;height:9px;border-radius:1px;overflow:hidden;background:var(--surface-2);gap:1px}
.seg{display:block;min-width:2px}
.legend{list-style:none;display:flex;flex-wrap:wrap;gap:5px 15px;margin:0;padding:0}
.legend li{display:flex;align-items:baseline;gap:5px;font-size:11.5px;color:var(--ink-mut)}
.dot{width:7px;height:7px;border-radius:50%;display:inline-block;transform:translateY(-1px)}
.lg-n{font-family:var(--mono);font-weight:600;color:var(--ink);font-variant-numeric:tabular-nums}
.s-done,.dot.s-done{background:var(--c-done)} .s-in_review,.dot.s-in_review{background:var(--c-in_review)}
.s-in_progress,.dot.s-in_progress{background:var(--c-in_progress)} .s-ready,.dot.s-ready{background:var(--c-ready)}
.s-design,.dot.s-design{background:var(--c-design)} .s-blocked,.dot.s-blocked{background:var(--c-blocked)}
.s-idea,.dot.s-idea{background:var(--c-idea)} .s-deferred,.dot.s-deferred{background:var(--c-deferred)}
.s-superseded,.dot.s-superseded{background:var(--c-superseded)}
.tscroll{overflow-x:auto}
.tscroll{margin:0 -22px;padding:0 22px;max-height:340px;overflow-y:auto}
table{width:100%;border-collapse:collapse;font-size:12.5px}
caption{text-align:left;font-family:var(--mono);font-size:10.5px;letter-spacing:.12em;text-transform:uppercase;
  color:var(--ink-mut);padding:8px 0 6px}
th{text-align:left;font-family:var(--mono);font-size:10px;letter-spacing:.1em;text-transform:uppercase;
  color:var(--ink-mut);font-weight:400;border-bottom:1px solid var(--rule-2);padding:5px 9px 5px 0;
  position:sticky;top:0;background:var(--surface)}
td{padding:6px 9px 6px 0;border-bottom:1px solid var(--rule);vertical-align:baseline;overflow-wrap:anywhere}
tr:last-child td{border-bottom:none}
td.id{font-family:var(--mono);font-weight:600;white-space:nowrap}
td.slug{color:var(--ink-mut);min-width:22ch}
.num{text-align:right;font-family:var(--mono);font-variant-numeric:tabular-nums;white-space:nowrap}
.chip{font-family:var(--mono);font-size:10px;letter-spacing:.04em;padding:2px 7px;border-radius:9px;
  white-space:nowrap;color:#fff}
.chip.s-idea,.chip.s-deferred,.chip.s-superseded{color:var(--ground)}
.pr{font-weight:600;text-decoration:none;border-bottom:1px solid var(--brass-soft)}
.pr.none{color:var(--rule-2);border:none}
.sess.zero{color:var(--rule-2)}
.grid{display:grid;gap:20px;grid-template-columns:repeat(auto-fit,minmax(min(430px,100%),1fr))}
@media(max-width:900px){.grid{grid-template-columns:minmax(0,1fr)}.tscroll{margin:0 -18px;padding:0 18px}}
footer{font-family:var(--mono);font-size:11px;color:var(--ink-mut);border-top:1px solid var(--rule);padding-top:14px;
  display:flex;justify-content:space-between;gap:14px;flex-wrap:wrap}
"##;

/// The shared page-reload script, inlined into every operator page this
/// crate renders. `build.rs` copies the same file to the Python package.
const PAGE_RELOAD_JS: &str = include_str!("page_reload.js");

/// The crown_ledger arm's beat: the age after which the /crown route in
/// crates/fno/src/web.rs starts its own render, so the file on disk and the
/// served page age alike.
pub const CROWN_LEDGER_INTERVAL_S: u64 = 300;

/// The arm as the daemon holds it: cadence stamp plus one-in-flight gate.
#[derive(Default)]
pub struct Arm {
    last_tick: std::sync::Mutex<Option<std::time::Instant>>,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// The production runner: the page render, cwd-bound (config, the graph and
/// the default out path all resolve per cwd).
fn run_ledger() -> Result<(), String> {
    let output = std::process::Command::new(crate::scrape::fno_py())
        .args(["agents", "king", "ledger"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !output.status.success() {
        let last = output
            .stderr
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .next_back()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .unwrap_or_default();
        return Err(format!(
            "exit {}: {last}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// One pass of the arm body: the run, then exactly one tick row - on every
/// path, so a silent tick cannot be told from one that never ran.
fn emit_one(
    home: &crate::paths::AgentsHome,
    run: impl FnOnce() -> Result<(), String>,
) -> crate::merge_close::CloseOutcome {
    let outcome = match run() {
        Ok(()) => crate::merge_close::CloseOutcome {
            acted: 1,
            skip_reason: None,
            detail: "reign.html rendered".to_string(),
        },
        Err(e) => crate::merge_close::CloseOutcome {
            acted: 0,
            skip_reason: Some("error".to_string()),
            detail: e.chars().take(200).collect(),
        },
    };
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "crown_ledger",
        crate::tick_ledger::SCHED_DAEMON,
        outcome.acted,
        outcome.skip_reason.as_deref(),
        Some(&outcome.detail),
        CROWN_LEDGER_INTERVAL_S,
    );
    outcome
}

/// The daemon-facing wrapper: due-check plus one-in-flight gate. A page
/// render is not dispatch, so there is no pause gate: the arm renders even
/// when no crown is live, so an empty court reads "no live crowns" rather
/// than a stale crown.
pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome) {
    maybe_tick_with(arm, home, run_ledger);
}

fn maybe_tick_with(
    arm: &Arm,
    home: crate::paths::AgentsHome,
    run: impl FnOnce() -> Result<(), String> + Send + 'static,
) {
    let interval = std::time::Duration::from_secs(CROWN_LEDGER_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < interval)
            || arm
                .in_flight
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let flag = std::sync::Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        emit_one(&home, run);
    });
}

/// `backlog.page_reload_s`: seconds between self-reloads of an open page.
/// Unset, negative, or not an integer reads as the 60-second default.
fn reload_secs(value: Option<toml::Value>) -> i64 {
    value
        .and_then(|v| v.as_integer())
        .filter(|s| *s >= 0)
        .unwrap_or(60)
}

/// The court arrives as a path or, with `-`, on stdin, so a relay never
/// needs a temp file to lose when a render is killed.
fn read_court(path: &Path, stdin: &mut dyn std::io::Read) -> Result<String, String> {
    if path.as_os_str() == "-" {
        let mut text = String::new();
        stdin
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        return Ok(text);
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// The whole page. Four verdict states; an unreadable registry and an empty
/// court are measurements, never a blank or falsely healthy page.
pub fn render(
    court: &Value,
    entries: &[Value],
    generated: &str,
    reload_s: i64,
    split_read: &Result<crate::crown_split::CrownSplits, String>,
) -> String {
    let projects: Result<HashMap<String, String>, String> =
        crate::king_board::project_map(&std::env::current_dir().unwrap_or_default());
    let summary = court.get("summary").cloned().unwrap_or(json!({}));
    let crowns = court.get("crowns").and_then(|c| c.as_array()).cloned();
    let titles = titles_of(entries);
    let gen = esc(generated);
    let mut out = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Crown Ledger</title>\
         <link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Fraunces:wght@500;700&family=JetBrains+Mono:wght@400;600&family=Public+Sans:wght@400;600&display=swap\">\
         <style>{CSS}</style></head><body><div class=\"wrap\">\
         <header class=\"masthead\">\
         <span class=\"eyebrow\">fno agents court · {gen} · <span class=\"age\" data-generated=\"{gen}\"></span></span>\
         <h1>Crown Ledger</h1>\
         <p class=\"dek\">Who rules which territory in the fleet, what each crown still owes, and whether the manifest and the registry tell the same story about any of it.</p>\
         </header>"
    );
    out.push_str(&verdict_card(court, &summary, split_read));
    if let Some(crowns) = &crowns {
        if !crowns.is_empty() {
            // Rungs: level ascending, null level last, court order within a
            // level; level 1 renders full width, the rest share the grid.
            let mut order: Vec<&Value> = crowns.iter().collect();
            order.sort_by_key(|c| {
                let l = c.get("level").and_then(|l| l.as_i64());
                (l.is_none(), l.unwrap_or(0))
            });
            let mut groups: Vec<(Option<i64>, Vec<&Value>)> = Vec::new();
            for c in order {
                let level = c.get("level").and_then(|l| l.as_i64());
                match groups.last_mut() {
                    Some((l, v)) if *l == level => v.push(c),
                    _ => groups.push((level, vec![c])),
                }
            }
            for (level, group) in &groups {
                out.push_str(&format!(
                    "<h2 class=\"sect\">{}</h2>",
                    rung_heading(*level, group.len())
                ));
                if *level == Some(1) {
                    for c in group {
                        out.push_str(&crown_card(c, &titles));
                    }
                } else {
                    out.push_str("<div class=\"grid\">");
                    for c in group {
                        out.push_str(&crown_card(c, &titles));
                    }
                    out.push_str("</div>");
                }
            }
        }
        // One compile per crown, shared by the uncrowned union: the page
        // renders the join twice otherwise.
        let compiled: Vec<Option<BTreeSet<String>>> = crowns
            .iter()
            .map(|c| members_of(c, entries, &projects))
            .collect();
        if !entries.is_empty() {
            out.push_str(&uncrowned_section(&compiled, entries));
            out.push_str(&orphan_leaves_section(entries));
        }
    }
    // Footer: crown count always (when the court is a list); the root
    // scope's size only when an L1 crown with a resolved fold exists.
    let mut parts: Vec<String> = Vec::new();
    if let Some(crowns) = &crowns {
        parts.push(format!(
            "{} {}",
            as_i64(&summary, "total"),
            plural(as_i64(&summary, "total"), "crown")
        ));
        let root = crowns
            .iter()
            .find(|c| c.get("level").and_then(|l| l.as_i64()) == Some(1));
        if let Some(root) = root {
            let fold = root.get("scope_nodes").cloned().unwrap_or(json!({}));
            if s_str(&fold, "status") != Some("unresolved") {
                parts.push(format!(
                    "{} nodes in the root scope",
                    as_i64(&fold, "total")
                ));
                parts.push(format!("{} active", active_sum(&fold)));
            }
        }
    }
    let right = if parts.is_empty() {
        String::new()
    } else {
        format!("<span>{}</span>", parts.join(" · "))
    };
    out.push_str(&format!(
        "<footer><span>generated from fno agents court -n</span>{right}</footer>"
    ));
    out.push_str(
        "</div><script>(function(){var el=document.querySelector(\".age\");if(!el)return;\
         var t=Date.parse(el.getAttribute(\"data-generated\"));if(isNaN(t))return;\
         var m=Math.floor((Date.now()-t)/60000);if(m<0)m=0;\
         el.textContent=m<60?m+\" min ago\":Math.floor(m/60)+\" h ago\";})();</script>",
    );
    out.push_str(&format!(
        "<script data-fno-reload=\"{reload_s}\">{PAGE_RELOAD_JS}</script></body></html>"
    ));
    out
}

pub(crate) fn write_atomic(path: &PathBuf, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    // The page travels over tailscale via `fno mux serve --web`'s `/crown`
    // route, so it must match graph.html's 600 regardless of umask - unlike
    // graph.html's Python writer, `std::fs::write` takes the umask-default
    // mode (644 under a typical 022 umask).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot chmod {}: {e}", tmp.display()));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot publish {}: {e}", path.display()));
    }
    Ok(())
}

pub fn run_reign_ledger(args: &[String]) -> i32 {
    let mut court_json: Option<PathBuf> = None;
    let mut graph: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut generated = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--court-json" if i + 1 < args.len() => {
                court_json = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--graph" if i + 1 < args.len() => {
                graph = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--generated" if i + 1 < args.len() => {
                generated = args[i + 1].clone();
                i += 2;
            }
            "--out" if i + 1 < args.len() => {
                out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => {
                eprintln!("fno-agents reign-ledger: unknown flag {other}");
                eprintln!(
                    "fno-agents reign-ledger: --court-json PATH|- --graph PATH \
                     --generated TS --out PATH"
                );
                return 2;
            }
        }
    }
    let (Some(court_path), Some(graph_path), Some(out_path)) = (court_json, graph, out) else {
        eprintln!("fno-agents reign-ledger: --court-json, --graph and --out are required");
        return 2;
    };
    let court_text = match read_court(&court_path, &mut std::io::stdin()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fno-agents reign-ledger: {e}");
            return 1;
        }
    };
    let court: Value = match serde_json::from_str(&court_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fno-agents reign-ledger: court JSON is not JSON: {e}");
            return 1;
        }
    };
    let entries: Vec<Value> = match crate::graph_store::read_rows(&graph_path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fno-agents reign-ledger: graph unreadable: {e}");
            return 1;
        }
    };
    let reload = reload_secs(crate::agents_config::config_lookup(
        &std::env::current_dir().unwrap_or_default(),
        &["backlog", "page_reload_s"],
    ));
    // The split read is the ledger's own registry read, once per render,
    // fed to the verdict card alongside the court payload.
    let split_read =
        crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
            .map(|registry| crate::crown_split::read_crown_splits(&registry.entries))
            .map_err(|e| e.to_string());
    if let Err(e) = write_atomic(
        &out_path,
        &render(&court, &entries, &generated, reload, &split_read),
    ) {
        eprintln!("fno-agents reign-ledger: {e}");
        return 1;
    }
    println!("reign ledger: {}", out_path.display());
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(court: Value, entries: Vec<Value>) -> String {
        render(
            &court,
            &entries,
            "2026-09-12T00:00:00Z",
            60,
            &Ok(crate::crown_split::CrownSplits::default()),
        )
    }

    fn base_crown() -> Value {
        json!({
            "holder": "king", "level": 2, "scope": "e-1", "grantor": "human",
            "status": "live", "agree": true, "reason": null, "crown_source": "both",
            "scope_nodes": {"status": "ok", "counts": {"in_progress": 1, "done": 2},
                "total": 3, "omitted": 1,
                "nodes": [{"id": "x-1", "status": "in_progress", "worker": "w1",
                           "pr_number": 7, "sessions": ["s1"]}]}
        })
    }

    fn base_court(crowns: Value) -> Value {
        json!({"crowns": crowns, "conflicts": [], "registry_readable": true,
               "graph_readable": true,
               "summary": {"total": 1, "manifest_only": 0, "sweep_ran": true,
                           "disagreements": 0, "unknowns": 0, "splits": 0}})
    }

    #[test]
    fn one_section_per_crown_names_scope_and_holder() {
        let court = base_court(json!([base_crown(), base_crown()]));
        let page = page(court, vec![]);
        assert_eq!(page.matches("<article class=\"crown").count(), 2);
        assert!(page.contains("e-1") && page.contains("king"));
    }

    #[test]
    fn unresolved_fold_states_its_reason_in_place() {
        let mut crown = base_crown();
        crown["scope_nodes"] = json!({"status": "unresolved", "reason": "boom"});
        let page = page(base_court(json!([crown])), vec![]);
        assert!(page.contains("scope fold: unresolved - boom"));
        assert!(!page.contains("<table"));
    }

    #[test]
    fn empty_court_renders_the_measurement() {
        let page = page(base_court(json!([])), vec![]);
        assert!(page.contains("no live crowns"));
        assert!(page.contains("verdict empty"));
    }

    #[test]
    fn registry_unreadable_names_the_reason() {
        let court = json!({"crowns": null, "registry_readable": false,
            "summary": {"reason": "registry unreadable: disk on fire"}});
        let page = page(court, vec![]);
        assert!(page.contains("The court cannot be read."));
        assert!(page.contains("registry unreadable: disk on fire"));
        assert!(page.contains("This is not an empty court; nothing was checked."));
        assert!(!page.contains("no live crowns"));
        assert!(!page.contains("class=\"facts\""));
    }

    #[test]
    fn hostile_fields_are_escaped() {
        let mut crown = base_crown();
        crown["holder"] = json!("<script>x</script>");
        crown["scope_nodes"]["nodes"] = json!([{"id": "x-1", "status": "in_progress",
            "worker": "<img>", "pr_number": 7}]);
        let page = page(base_court(json!([crown])), vec![]);
        // The page carries its own inline age script, so the hostile payload
        // is what must stay raw-HTML-free, not the script tag itself.
        assert!(!page.contains("<script>x</script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<img"));
    }

    #[test]
    fn counts_render_in_lifecycle_order() {
        let mut crown = base_crown();
        crown["scope_nodes"]["counts"] = json!({"zebra": 1, "done": 2, "in_progress": 1});
        let page = page(base_court(json!([crown])), vec![]);
        let legend = &page[page.find("<ul class=\"legend\">").unwrap()..];
        let ip = legend.find(">in progress<").unwrap();
        let dn = legend.find(">done<").unwrap();
        let zb = legend.find(">zebra<").unwrap();
        assert!(ip < dn && dn < zb);
    }

    #[test]
    fn uncrowned_epics_get_their_own_section() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "title": "reigned epic", "status": "ready", "priority": "p2"}),
            json!({"id": "x-9", "parent": "e-1", "title": "contained", "status": "in_progress"}),
            json!({"id": "e-2", "type": "epic", "title": "free one", "status": "ready", "priority": "p2"}),
            json!({"id": "e-3", "type": "epic", "title": "urgent orphan", "status": "idea", "priority": "p1"}),
        ];
        let whole = page(base_court(json!([base_crown()])), entries);
        let at = whole.find("uncrowned epics").expect("uncrowned section");
        let section = &whole[at..];
        assert!(section.contains("2 uncrowned, 1 at p1"));
        assert!(section.contains("free one") && section.contains("urgent orphan"));
        assert!(!section.contains("reigned epic"));
    }

    #[test]
    fn orphan_leaves_follow_the_structural_rule() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "parent": null, "status": "in_progress"}),
            json!({"id": "x-1", "parent": "e-1", "status": "in_progress", "title": "contained", "priority": "p1"}),
            json!({"id": "l-1", "title": "free leaf", "status": "ready", "priority": "p1"}),
            json!({"id": "l-2", "title": "done leaf", "status": "done", "priority": "p1"}),
            json!({"id": "l-3", "title": "container", "status": "in_progress", "priority": "p2"}),
            json!({"id": "l-4", "parent": "l-3", "status": "ready"}),
            json!({"id": "l-5", "title": "empty parent leaf", "parent": "", "status": "ready", "priority": "p2"}),
        ];
        let whole = page(base_court(json!([base_crown()])), entries);
        let at = whole.find("orphan leaves").expect("leaves section");
        let section = &whole[at..];
        assert!(section.contains("2 orphan leaves, 1 at p1"));
        assert!(section.contains("free leaf"));
        assert!(!section.contains("done leaf"));
        assert!(!section.contains("container"));
        assert!(!section.contains("contained"));
    }

    #[test]
    fn writes_atomically_and_names_the_path() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("reign-ledger-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        let court_path = dir.join("court.json");
        let graph_path = dir.join("graph.json");
        let out_path = dir.join("reign.html");
        std::fs::write(&court_path, base_court(json!([base_crown()])).to_string()).unwrap();
        std::fs::write(&graph_path, json!({"entries": []}).to_string()).unwrap();
        let args: Vec<String> = [
            "--court-json",
            court_path.to_str().unwrap(),
            "--graph",
            graph_path.to_str().unwrap(),
            "--generated",
            "2026-09-12T00:00:00Z",
            "--out",
            out_path.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_reign_ledger(&args), 0);
        assert!(out_path.exists());
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftovers);
        std::env::remove_var("FNO_AGENTS_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn publishes_reign_html_at_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("reign-ledger-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", &dir);
        let court_path = dir.join("court.json");
        let graph_path = dir.join("graph.json");
        let out_path = dir.join("reign.html");
        std::fs::write(&court_path, base_court(json!([base_crown()])).to_string()).unwrap();
        std::fs::write(&graph_path, json!({"entries": []}).to_string()).unwrap();
        let args: Vec<String> = [
            "--court-json",
            court_path.to_str().unwrap(),
            "--graph",
            graph_path.to_str().unwrap(),
            "--generated",
            "2026-09-12T00:00:00Z",
            "--out",
            out_path.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_reign_ledger(&args), 0);
        let mode = std::fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "reign.html must match graph.html's 600 mode");
        std::env::remove_var("FNO_AGENTS_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_declares_the_device_viewport() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page
            .contains("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"));
    }

    #[test]
    fn cells_wrap_words_not_characters() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains("overflow-wrap:anywhere"));
        assert!(!page.contains("word-break:break-all"));
    }

    #[test]
    fn every_table_scrolls_inside_its_own_container() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains(".tscroll{overflow-x:auto}"));
        assert_eq!(page.matches("<div class=\"tscroll\"><table>").count(), 1);
    }

    #[test]
    fn statuses_render_as_pills() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains("<span class=\"chip s-in_progress\">in progress</span>"));
    }

    #[test]
    fn masthead_carries_the_generated_stamp_and_age_hook() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains("<h1>Crown Ledger</h1>"));
        assert!(page.contains("fno agents court · 2026-09-12T00:00:00Z"));
        assert!(page.contains("<span class=\"age\" data-generated=\"2026-09-12T00:00:00Z\">"));
        assert!(page.contains("Date.parse(el.getAttribute(\"data-generated\"))"));
    }

    #[test]
    fn verdict_agrees_when_the_counts_are_all_zero() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains("The court agrees with itself."));
        assert!(page.contains("<span class=\"mark\">✓</span>"));
        assert!(page.contains(
            "<div class=\"fact\"><span class=\"fv\">1</span><span class=\"fl\">crowns</span></div>"
        ));
    }

    #[test]
    fn double_rule_flips_the_verdict_and_marks_the_tile_bad() {
        let splits = Ok(crate::crown_split::CrownSplits {
            double_ruled: vec![crate::crown_split::ScopeSplit {
                scope: "shared".into(),
                holders: vec!["king-a".into(), "king-b".into()],
            }],
            stale: Vec::new(),
        });
        let page = render(
            &base_court(json!([base_crown()])),
            &[],
            "2026-09-12T00:00:00Z",
            60,
            &splits,
        );
        assert!(page.contains("The court disagrees with itself."));
        assert!(page.contains(
            "<div class=\"fact bad\"><span class=\"fv\">1</span><span class=\"fl\">double ruled</span></div>"
        ));
    }

    #[test]
    fn stale_crowns_mark_their_tile_without_flipping_the_verdict() {
        let splits = Ok(crate::crown_split::CrownSplits {
            double_ruled: Vec::new(),
            stale: vec![crate::crown_split::StaleCrown {
                row: "king-dead".into(),
                scope: "shared".into(),
                stored_status: "orphaned".into(),
            }],
        });
        let page = render(
            &base_court(json!([base_crown()])),
            &[],
            "2026-09-12T00:00:00Z",
            60,
            &splits,
        );
        assert!(page.contains("The court agrees with itself."));
        assert!(page.contains(
            "<div class=\"fact bad\"><span class=\"fv\">1</span><span class=\"fl\">stale crowns</span></div>"
        ));
    }

    #[test]
    fn an_unread_registry_renders_dashes_with_the_reason_never_zero() {
        let page = render(
            &base_court(json!([base_crown()])),
            &[],
            "2026-09-12T00:00:00Z",
            60,
            &Err("crown split read failed: boom".to_string()),
        );
        assert!(page.contains(
            "<div class=\"fact\"><span class=\"fv\">-</span><span class=\"fl\">double ruled</span></div>"
        ));
        assert!(page.contains(
            "<div class=\"fact\"><span class=\"fv\">-</span><span class=\"fl\">stale crowns</span></div>"
        ));
        assert!(page.contains("crown split read failed: boom"));
    }

    #[test]
    fn verdict_disagrees_and_marks_bad_facts() {
        let mut court = base_court(json!([base_crown()]));
        court["summary"]["disagreements"] = json!(1);
        let page = page(court, vec![]);
        assert!(page.contains("The court disagrees with itself."));
        assert!(page.contains("verdict bad"));
        assert!(page.contains(
            "<div class=\"fact bad\"><span class=\"fv\">1</span><span class=\"fl\">disagreements</span></div>"
        ));
    }

    #[test]
    fn null_counts_render_question_marks_not_zeroes() {
        let mut court = base_court(json!([base_crown()]));
        court["summary"]["splits"] = Value::Null;
        let page = page(court, vec![]);
        assert!(page.contains(
            "<div class=\"fact\"><span class=\"fv\">?</span><span class=\"fl\">splits</span></div>"
        ));
    }

    #[test]
    fn readers_line_prints_unknown_for_null() {
        let mut court = base_court(json!([base_crown()]));
        court["graph_readable"] = Value::Null;
        let page = page(court, vec![]);
        assert!(page.contains("graph_readable unknown · registry_readable true · sweep_ran true"));
    }

    #[test]
    fn rungs_group_crowns_by_level_with_headings() {
        let mut l2a = base_crown();
        l2a["scope"] = json!("e-a");
        let mut l2b = base_crown();
        l2b["scope"] = json!("e-b");
        let mut l1 = base_crown();
        l1["level"] = json!(1);
        l1["scope"] = json!("fno");
        let page = page(base_court(json!([l2a, l1, l2b])), vec![]);
        let h1 = page
            .find("Rung 1 - the whole project")
            .expect("rung 1 heading");
        let root = page.find("class=\"crown root\"").expect("root card");
        let h2 = page
            .find("Rung 2 - 2 territories granted beneath it")
            .expect("rung 2 heading");
        assert!(h1 < root && root < h2);
        let la = page.find("<h2>e-a</h2>").unwrap();
        let lb = page.find("<h2>e-b</h2>").unwrap();
        assert!(la < lb);
    }

    #[test]
    fn crown_card_shows_stats_bar_and_legend() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains(
            "<span><b>3</b> nodes</span><span><b>1</b> active</span><span><b>2</b> done</span>"
        ));
        assert!(page.contains("style=\"flex:1\""));
        assert!(page.contains("style=\"flex:2\""));
        assert!(page.contains("aria-label=\"in progress 1, done 2\""));
    }

    #[test]
    fn work_cell_uses_slug_words_with_the_title_as_tip() {
        let mut crown = base_crown();
        crown["scope_nodes"]["nodes"] = json!([{"id": "x-1", "status": "in_progress",
            "slug": "x-1-add-the-thing", "sessions": ["s1"]}]);
        let entries =
            vec![json!({"id": "x-1", "slug": "x-1-add-the-thing", "title": "Add the thing"})];
        let page = page(base_court(json!([crown])), entries);
        assert!(page.contains("<td class=\"slug\" title=\"Add the thing\">x 1 add the thing</td>"));
    }

    #[test]
    fn pr_links_only_https_urls() {
        let mut crown = base_crown();
        crown["scope_nodes"]["nodes"] = json!([
            {"id": "x-1", "status": "in_progress", "pr_number": 7, "sessions": []},
            {"id": "x-2", "status": "ready", "pr_number": 8, "sessions": []},
            {"id": "x-3", "status": "blocked", "sessions": []}
        ]);
        let entries = vec![
            json!({"id": "x-1", "pr_url": "javascript:alert(1)"}),
            json!({"id": "x-2", "pr_url": "https://github.com/o/r/pull/8"}),
        ];
        let page = page(base_court(json!([crown])), entries);
        assert!(page.contains("<td class=\"num\">#7</td>"));
        assert!(!page.contains("javascript:alert"));
        assert!(page.contains("<a class=\"pr\" href=\"https://github.com/o/r/pull/8\">#8</a>"));
        assert!(page.contains("<span class=\"pr none\">-</span>"));
    }

    #[test]
    fn font_stacks_keep_system_fallbacks() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains("\"JetBrains Mono\",ui-monospace,SFMono-Regular,Menlo,monospace"));
        assert!(page.contains("\"Public Sans\",system-ui,-apple-system,Segoe UI,sans-serif"));
        assert!(page.contains("\"Fraunces\",\"Iowan Old Style\",Georgia,serif"));
    }

    #[test]
    fn grid_fits_a_narrow_phone() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        assert!(page.contains("grid-template-columns:repeat(auto-fit,minmax(min(430px,100%),1fr))"));
    }

    #[test]
    fn page_contains_no_em_dash() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "title": "free one", "status": "ready", "priority": "p2"}),
        ];
        let page = page(base_court(json!([base_crown(), base_crown()])), entries);
        assert!(!page.contains('\u{2014}'));
    }

    #[test]
    fn page_carries_one_reload_script_before_body_close() {
        let page = page(base_court(json!([base_crown()])), vec![]);
        let tag = "<script data-fno-reload=\"60\">";
        assert_eq!(page.matches(tag).count(), 1, "exactly one reload tag");
        let tail = format!("{tag}{PAGE_RELOAD_JS}</script></body></html>");
        assert!(
            page.ends_with(&tail),
            "the reload script is the last thing before the page close"
        );
    }

    #[test]
    fn reload_interval_reads_60_unless_a_whole_non_negative_number() {
        assert_eq!(reload_secs(None), 60);
        assert_eq!(reload_secs(Some(toml::Value::Integer(-1))), 60);
        assert_eq!(reload_secs(Some(toml::Value::Float(90.5))), 60);
        assert_eq!(reload_secs(Some(toml::Value::String("90".into()))), 60);
        assert_eq!(reload_secs(Some(toml::Value::Integer(0))), 0);
        assert_eq!(reload_secs(Some(toml::Value::Integer(120))), 120);
    }

    #[test]
    fn court_json_dash_reads_the_court_from_stdin() {
        let text = read_court(
            &PathBuf::from("-"),
            &mut std::io::Cursor::new("{\"crowns\":[]}"),
        )
        .unwrap();
        assert_eq!(text, "{\"crowns\":[]}");
        let dir = std::env::temp_dir().join(format!("reign-court-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("court.json");
        std::fs::write(&path, "{\"crowns\":[1]}").unwrap();
        let from_file = read_court(&path, &mut std::io::empty()).unwrap();
        assert_eq!(from_file, "{\"crowns\":[1]}");
        let err = read_court(&dir.join("absent.json"), &mut std::io::empty()).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn home() -> crate::paths::AgentsHome {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("crown-ledger-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::paths::AgentsHome::at(dir.join("home"))
    }

    #[test]
    fn crown_ledger_success_writes_one_acted_row() {
        let h = home();
        let o = emit_one(&h, || Ok(()));
        assert_eq!(o.acted, 1);
        assert_eq!(o.skip_reason, None);
        let log = std::fs::read_to_string(h.events_jsonl()).unwrap_or_default();
        assert_eq!(
            log.matches("\"arm\":\"crown_ledger\"").count(),
            1,
            "log: {log}"
        );
        assert!(log.contains("\"acted\":1"), "log: {log}");
        assert!(log.contains("\"interval_s\":300"), "log: {log}");
    }

    #[test]
    fn crown_ledger_failure_is_an_error_row() {
        let h = home();
        let o = emit_one(&h, || Err("exit 1: graph unreadable".to_string()));
        assert_eq!(o.acted, 0);
        assert_eq!(o.skip_reason.as_deref(), Some("error"));
        let log = std::fs::read_to_string(h.events_jsonl()).unwrap_or_default();
        assert!(log.contains("\"acted\":0"), "log: {log}");
        assert!(log.contains("\"skip_reason\":\"error\""), "log: {log}");
        assert!(log.contains("graph unreadable"), "log: {log}");
    }

    #[test]
    fn crown_ledger_young_cadence_stamp_runs_nothing() {
        let arm = Arm::default();
        let h = home();
        *arm.last_tick.lock().unwrap() = Some(std::time::Instant::now());
        maybe_tick_with(&arm, h.clone(), || panic!("arm must be gated"));
        assert!(!h.events_jsonl().exists(), "a gated tick wrote no row");
    }
}

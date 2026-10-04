//! The one branch mint, resolver and parser for node branches.
//!
//! A node's branch is `<kind>/<node>-<mini-slug>` (kind: `feature/`,
//! `bugfix/`, `chore/`); the legacy `feature/<node>` shape stays readable,
//! so nothing needs a sunset. The mint drops slug words that would parse as
//! a second node id, which makes the round trip exact:
//! `node_ids(mint(row)) == [id]` always. The resolver prefers a branch the
//! work already lives on, so an open PR is never renamed.

use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// Delimiter-bounded node-id candidates of a head ref. Hand-rolled: the
/// pattern needs lookaheads (`(?=$|[/-])`) that the regex crate does not
/// support, including the compact legacy `x` form.
pub(crate) fn node_ids(head_ref: &str) -> Vec<String> {
    let b = head_ref.as_bytes();
    let mut ids: Vec<String> = Vec::new();
    // Non-overlapping left-to-right scan, exactly like Python's finditer: a
    // match is consumed and the scan resumes after it, so "feature/x-aaaa-1234"
    // never yields the bogus "cdef-1234" from inside the first match's tail.
    let mut i = 0;
    while i < b.len() {
        // A candidate starts at the string head or after '-' / '/'.
        if !(i == 0 || b[i - 1] == b'-' || b[i - 1] == b'/') {
            i += 1;
            continue;
        }
        if !b[i].is_ascii_lowercase() {
            i += 1;
            continue;
        }
        // [a-z][a-z0-9]{0,7} then an optional '-' then [0-9a-f]{4,8}: the
        // dash-less minter shape stays a candidate. The run bounds the whole
        // body (prefix 1-8 + hex 4-8 = 16), and each branch re-checks its own
        // prefix/hex lengths.
        let mut j = i + 1;
        let mut alnum = 0;
        while j < b.len() && alnum < 15 && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit()) {
            j += 1;
            alnum += 1;
        }
        let mut matched: Option<(usize, usize)> = None; // (hex_start, hex_end)
        if j < b.len() && b[j] == b'-' && j - i - 1 <= 7 {
            // Dashed: prefix ran 1-8 chars, then '-' then 4-8 hex.
            let hex_start = j + 1;
            let mut k = hex_start;
            while k < b.len()
                && k - hex_start < 8
                && (b[k].is_ascii_digit() || (b'a'..=b'f').contains(&b[k]))
            {
                k += 1;
            }
            let hex_len = k - hex_start;
            if (4..=8).contains(&hex_len) && (k == b.len() || b[k] == b'-' || b[k] == b'/') {
                matched = Some((hex_start, k));
            }
        }
        if matched.is_none() {
            // Compact: the alnum run's own tail is the hex, greedy head first
            // (shortest hex, head 1-8 chars); the run end is a boundary or EOL.
            let min_tail = 4.max((j - i).saturating_sub(8));
            for tail in min_tail..=(8.min(alnum)) {
                let hex_start = j - tail;
                let hex_ok = b[hex_start..j]
                    .iter()
                    .all(|&c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
                if hex_ok && (j == b.len() || b[j] == b'-' || b[j] == b'/') {
                    matched = Some((hex_start, j));
                    break;
                }
            }
        }
        let Some((_, hex_end)) = matched else {
            i += 1;
            continue;
        };
        let candidate = &head_ref[i..hex_end];
        if !ids.iter().any(|c| c == candidate) {
            ids.push(candidate.to_string());
        }
        i = hex_end;
    }
    ids
}

/// The branch kind of a node row: `bugfix` for type bug, `chore` for type
/// chore or docs or a docs domain, `feature` for everything else (feature,
/// epic, task, refactor, roadmap, missing).
pub(crate) fn kind(row: &Value) -> &'static str {
    let t = row.get("type").and_then(Value::as_str).unwrap_or("");
    if t == "bug" {
        return "bugfix";
    }
    if t == "chore" || t == "docs" {
        return "chore";
    }
    if row.get("domain").and_then(Value::as_str) == Some("docs") {
        return "chore";
    }
    "feature"
}

fn row_slug(row: &Value) -> &str {
    row.get("slug").and_then(Value::as_str).unwrap_or("")
}

/// The mini-slug words after the node id: the slug's own words, sanitized to
/// `[a-z0-9]`, capped at 4 words and a 30-char joined tail. A leading
/// `<id>-` is stripped; a slug equal to the id (or empty) yields nothing. A
/// first word over 30 chars is cut to 30.
fn mini_words(id: &str, slug: &str) -> Vec<String> {
    let mut s = slug.trim().to_lowercase();
    if s == id {
        return Vec::new();
    }
    if let Some(rest) = s.strip_prefix(id).and_then(|r| r.strip_prefix('-')) {
        s = rest.to_string();
    }
    let mut words: Vec<String> = Vec::new();
    for raw in s.split('-') {
        let w: String = raw
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .collect();
        if w.is_empty() {
            continue;
        }
        if words.is_empty() {
            words.push(w.chars().take(30).collect());
            continue;
        }
        if words.len() >= 4 || words.join("-").len() + 1 + w.len() > 30 {
            break;
        }
        words.push(w);
    }
    words
}

/// The branch a node's work mints onto: `<kind>/<id>-<mini-slug>`, or
/// `<kind>/<id>` with no usable slug. Slug words that would parse as a
/// second node id are dropped from the tail, then a trailing one-character
/// word goes too, so `node_ids(mint(row)) == [id]` always. `None` without
/// an id.
pub(crate) fn mint(row: &Value) -> Option<String> {
    let id = row.get("id").and_then(Value::as_str)?;
    let k = kind(row);
    let mut words = mini_words(id, row_slug(row));
    let named = |words: &[String]| {
        if words.is_empty() {
            format!("{k}/{id}")
        } else {
            format!("{k}/{id}-{}", words.join("-"))
        }
    };
    loop {
        if node_ids(&named(&words)) == vec![id.to_string()] {
            break;
        }
        if words.pop().is_none() {
            break;
        }
    }
    if words.last().is_some_and(|w| w.chars().count() == 1) {
        words.pop();
    }
    Some(named(&words))
}

/// The branch names a node's work may already live on, in preference order:
/// the mint (the guarded name), the legacy `feature/<id>`, then the other
/// two kinds with the same mini tail. Nothing else: a hand-made side branch
/// (`feature/<id>-w2`) is never adopted.
pub(crate) fn accepted(row: &Value) -> Vec<String> {
    let Some(id) = row.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let k = kind(row);
    let tail = mini_words(id, row_slug(row)).join("-");
    let named = |k: &str| {
        if tail.is_empty() {
            format!("{k}/{id}")
        } else {
            format!("{k}/{id}-{tail}")
        }
    };
    let first = mint(row).unwrap_or_else(|| named(k));
    let mut out = vec![first, format!("feature/{id}")];
    for other in ["feature", "bugfix", "chore"] {
        if other != k {
            out.push(named(other));
        }
    }
    out.dedup();
    out
}

/// The branch this node's work should run on: an already-present accepted
/// name (local heads first, then origin), else the mint. A missing repo, a
/// git failure or a timeout reads as no refs, so the mint answers.
pub(crate) fn resolve(row: &Value) -> Option<String> {
    let accepted = accepted(row);
    if accepted.is_empty() {
        return None;
    }
    let minted = accepted.first().cloned();
    let repo = row
        .get("_resolved_cwd")
        .or_else(|| row.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if repo.is_empty() {
        return minted;
    }
    let cmd = vec![
        "git".to_string(),
        "-C".to_string(),
        repo.to_string(),
        "for-each-ref".to_string(),
        "--format=%(refname)".to_string(),
        "refs/heads".to_string(),
        "refs/remotes/origin".to_string(),
    ];
    let refs = match crate::org_board::budget::run_with_timeout(
        &cmd,
        Path::new("."),
        Duration::from_secs(5),
    ) {
        Ok(out) => String::from_utf8_lossy(&out).into_owned(),
        Err(_) => return minted,
    };
    let names: HashSet<&str> = refs.lines().collect();
    let wanted = |prefix: &str| {
        accepted
            .iter()
            .find(|n| names.contains(format!("{prefix}{n}").as_str()))
            .cloned()
    };
    wanted("refs/heads/")
        .or_else(|| wanted("refs/remotes/origin/"))
        .or(minted)
}

/// The node id a short ref names, when the ref reads `<kind>/<id>` or
/// `<kind>/<id>-...` for a kind in `feature|bugfix|chore`, with the id
/// being the first `node_ids` candidate at the segment start.
pub(crate) fn owner(short_ref: &str) -> Option<String> {
    let rest = short_ref.split_once('/')?.1;
    let candidate = node_ids(rest).into_iter().next()?;
    if !rest.starts_with(&candidate) {
        return None;
    }
    let after = &rest[candidate.len()..];
    if after.is_empty() || after.starts_with('-') {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, typ: &str, slug: &str) -> Value {
        json!({"id": id, "type": typ, "slug": slug})
    }

    fn feature(id: &str, slug: &str) -> Value {
        node(id, "feature", slug)
    }

    // -- the parser -----------------------------------------------------------

    #[test]
    fn ids_never_match_a_partial_hex_prefix() {
        assert_eq!(node_ids("feature/x-aaaa-1234"), vec!["x-aaaa".to_string()]);
        assert_eq!(
            node_ids("x-5b667-fixes-x-bbbb"),
            vec!["x-5b667".to_string(), "x-bbbb".to_string()]
        );
        // Uppercase is not id body ([0-9a-f], not [0-9a-fA-F]): the hex run
        // stops at 'E', so "x-cccc" binds and the tail never reads as id.
        assert_eq!(node_ids("x-cccc-EF12"), vec!["x-cccc".to_string()]);
        // Dash-less minter shape: prefix + hex as one segment.
        assert_eq!(node_ids("feature/xbbbb"), vec!["xbbbb".to_string()]);
        assert_eq!(node_ids("xbbbb-fix"), vec!["xbbbb".to_string()]);
        assert!(node_ids("main").is_empty());
    }

    #[test]
    fn ids_accept_compact_legacy_ids_at_segment_boundaries() {
        assert_eq!(node_ids("feature/xd863"), vec!["xd863"]);
        assert_eq!(node_ids("feature/xd863-close"), vec!["xd863"]);
        assert!(node_ids("feature/xd863g").is_empty());
        assert!(node_ids("feature/xg863").is_empty());
    }

    #[test]
    fn ids_scan_any_kind_prefix_and_fixed_width_hex() {
        assert_eq!(node_ids("feature/x-1179a"), vec!["x-1179a"]);
        assert_eq!(node_ids("x-7b9cd"), vec!["x-7b9cd"]);
        assert_eq!(node_ids("bugfix/x-7aafb-repro"), vec!["x-7aafb"]);
        assert_eq!(node_ids("feature/x-5b667"), vec!["x-5b667"]);
        assert_eq!(node_ids("x-ab123-x-cd456"), vec!["x-ab123", "x-cd456"]);
        assert!(node_ids("fix/thing").is_empty());
    }

    // -- the kind map -----------------------------------------------------------

    #[test]
    fn the_kind_map_routes_type_and_docs_domain() {
        assert_eq!(kind(&node("x-eeee", "bug", "s")), "bugfix");
        assert_eq!(kind(&node("x-eeee", "chore", "s")), "chore");
        assert_eq!(kind(&node("x-eeee", "docs", "s")), "chore");
        let mut docs_domain = node("x-eeee", "feature", "s");
        docs_domain["domain"] = json!("docs");
        assert_eq!(kind(&docs_domain), "chore");
        assert_eq!(kind(&node("x-eeee", "epic", "s")), "feature");
        assert_eq!(kind(&node("x-eeee", "task", "s")), "feature");
        assert_eq!(kind(&node("x-eeee", "refactor", "s")), "feature");
    }

    // -- the mint ---------------------------------------------------------------

    #[test]
    fn mint_keeps_four_words_and_the_id_whole() {
        let row = feature("x-aaaa", "install-channels-for-the-cli");
        assert_eq!(
            mint(&row).unwrap(),
            "feature/x-aaaa-install-channels-for-the"
        );
        assert_eq!(node_ids(&mint(&row).unwrap()), vec!["x-aaaa".to_string()]);
    }

    #[test]
    fn mint_caps_the_joined_tail_at_thirty_chars() {
        let row = feature("x-aaaa", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-b");
        assert_eq!(
            mint(&row).unwrap(),
            "feature/x-aaaa-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        // A first word over the cap is cut to 30.
        let long = feature("x-aaaa", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-b");
        assert_eq!(
            mint(&long).unwrap(),
            "feature/x-aaaa-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn mint_drops_slug_words_that_parse_as_node_ids() {
        // "fix-dead" and "beef" both parse as ids; they drop from the tail.
        let row = feature("x-aaaa", "fix-dead-beef");
        assert_eq!(mint(&row).unwrap(), "feature/x-aaaa-fix");
        // "added" holds the hex tail "dded"; the whole tail drops.
        let row = feature("x-aaaa", "added-cache");
        assert_eq!(mint(&row).unwrap(), "feature/x-aaaa");
    }

    #[test]
    fn mint_drops_a_slug_word_holding_another_id_and_a_trailing_one_char_word() {
        let row = feature("x-aaaa", "extend-reverse-map-x-bbbb");
        assert_eq!(mint(&row).unwrap(), "feature/x-aaaa-extend-reverse-map");
        let row = feature("x-bbbb", "bug-w3-x-dddd-unified-only");
        assert_eq!(mint(&row).unwrap(), "feature/x-bbbb-bug-w3");
    }

    #[test]
    fn mint_degrades_to_the_bare_kind_id_shape() {
        assert_eq!(
            mint(&feature("x-aaaa", "x-aaaa")).unwrap(),
            "feature/x-aaaa"
        );
        assert_eq!(mint(&feature("x-aaaa", "")).unwrap(), "feature/x-aaaa");
    }

    #[test]
    fn mint_answers_none_without_an_id() {
        assert_eq!(mint(&json!({"type": "feature"})), None);
        assert!(accepted(&json!({})).is_empty());
        assert_eq!(resolve(&json!({})), None);
    }

    #[test]
    fn every_fixture_mint_round_trips_to_its_node() {
        let fixtures = [
            feature("x-aaaa", "install-channels-for-the-cli"),
            feature("x-aaaa", "fix-dead-beef"),
            feature("x-aaaa", "added-cache"),
            feature("x-aaaa", "extend-reverse-map-x-bbbb"),
            feature("x-bbbb", "bug-w3-x-dddd-unified-only"),
            feature("x-aaaa", "x-aaaa"),
            feature("x-aaaa", ""),
            node("x-eeee", "bug", "wrong-close-on-the-board"),
            node("x-eeee", "chore", "sweep-the-dead-minters"),
            node("xd863", "feature", "compact-legacy-id"),
        ];
        for row in fixtures {
            let name = mint(&row).unwrap();
            let id = row["id"].as_str().unwrap();
            assert_eq!(node_ids(&name), vec![id.to_string()], "for {name}");
        }
    }

    // -- accepted + resolve -----------------------------------------------------

    #[test]
    fn accepted_lists_the_mint_the_legacy_and_the_other_kinds() {
        let row = node("x-eeee", "bug", "wrong-close-on-the-board");
        assert_eq!(
            accepted(&row),
            vec![
                "bugfix/x-eeee-wrong-close-on-the",
                "feature/x-eeee",
                "feature/x-eeee-wrong-close-on-the",
                "chore/x-eeee-wrong-close-on-the",
            ]
        );
        // A kind change keeps the legacy bare shape as its own entry once.
        let bare = feature("x-aaaa", "");
        assert_eq!(
            accepted(&bare),
            vec!["feature/x-aaaa", "bugfix/x-aaaa", "chore/x-aaaa"]
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git ran");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seed_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.py"), "x = 1\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "seed"]);
        (tmp, repo)
    }

    fn row_at(repo: &Path) -> Value {
        json!({"id": "x-eeee", "type": "feature", "slug": "some-work", "cwd": repo})
    }

    #[test]
    fn resolve_prefers_local_then_origin_then_the_mint() {
        let (_tmp, repo) = seed_repo();
        let row = row_at(&repo);
        let minted = mint(&row).unwrap();
        // No refs: the mint.
        assert_eq!(resolve(&row).unwrap(), minted);

        // A local legacy branch is reused.
        git(&repo, &["branch", "feature/x-eeee"]);
        assert_eq!(resolve(&row).unwrap(), "feature/x-eeee");

        // Origin-only counts too; the local branch outranks it.
        git(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/feature/x-eeee-some-work",
                "HEAD",
            ],
        );
        assert_eq!(resolve(&row).unwrap(), "feature/x-eeee");
        git(&repo, &["branch", "-D", "feature/x-eeee"]);
        assert_eq!(resolve(&row).unwrap(), "feature/x-eeee-some-work");
    }

    #[test]
    fn resolve_never_adopts_a_hand_made_side_branch() {
        let (_tmp, repo) = seed_repo();
        let row = row_at(&repo);
        git(&repo, &["branch", "feature/x-eeee-w2"]);
        assert_eq!(resolve(&row).unwrap(), "feature/x-eeee-some-work");
    }

    #[test]
    fn resolve_reuses_a_kind_changed_branch_with_the_same_tail() {
        let (_tmp, repo) = seed_repo();
        let mut row = row_at(&repo);
        git(&repo, &["branch", "feature/x-eeee-some-work"]);
        row["type"] = json!("bug");
        assert_eq!(resolve(&row).unwrap(), "feature/x-eeee-some-work");
    }

    #[test]
    fn resolve_without_a_repo_answers_the_mint() {
        let row = json!({"id": "x-eeee", "type": "feature", "slug": "some-work"});
        assert_eq!(resolve(&row).unwrap(), "feature/x-eeee-some-work");
    }

    // -- the owner check ----------------------------------------------------------

    #[test]
    fn owner_names_the_id_only_from_a_kind_segment() {
        assert_eq!(owner("feature/x-eeee"), Some("x-eeee".to_string()));
        assert_eq!(owner("feature/x-eeee-w2"), Some("x-eeee".to_string()));
        assert_eq!(
            owner("bugfix/x-eeee-wrong-close"),
            Some("x-eeee".to_string())
        );
        assert_eq!(owner("main"), None);
        assert_eq!(owner("feature/main"), None);
        // The leftmost match must START the id segment, not sit inside it.
        assert_eq!(owner("feature/fix-x-eeee"), None);
        // <kind>/<id>-... is an owner shape whatever follows the tail.
        assert_eq!(owner("feature/x-eeee-w2/inner"), Some("x-eeee".to_string()));
    }
}

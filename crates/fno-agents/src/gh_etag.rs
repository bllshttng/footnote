//! Conditional GETs for the status door: an eligible `gh api` read goes out
//! once as `gh api -i` with the row's ETag in `If-None-Match`, and a 304 is
//! served from the local row. A 304 still costs a request to GitHub's
//! secondary limiter, but it does not spend core quota - the cure this
//! door can be, before the one-poller slice lowers the request rate.
//! A missing, corrupt, or stale row is a miss, never an error: the plain
//! network read stays the truth (the gh_cache.rs rule).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Process-wide count of 304s the door served; the spend notes read it.
pub(crate) static NOT_MODIFIED: AtomicUsize = AtomicUsize::new(0);

/// How long a row may seed If-None-Match. After this the row is a miss, so
/// a dead head sha cannot pin stale bodies forever.
const ROW_TTL_S: f64 = 24.0 * 3600.0;

/// A row stamped further than this into the future is a miss too: clock
/// skew or a copied state dir must not pin a row past the 24h bound.
const CLOCK_SLACK_S: f64 = 60.0;

/// The argv shapes the door serves: exactly `["api", path]` where path is a
/// plain REST read under `repos/`. A flag in the second token (write,
/// method, paginate, jq - anything starting `-`) fails the prefix check, so
/// writes never qualify; `graphql` is not under `repos/`. A `{` in the path
/// is a gh template placeholder gh substitutes from the cwd, so the row key
/// would name a resource other than the one read; those stay plain.
pub(crate) fn eligible(args: &[String]) -> Option<&str> {
    let [bin, path] = args else {
        return None;
    };
    if bin != "api" {
        return None;
    }
    let path = path.as_str();
    if !path.starts_with("repos/")
        || path.contains("/logs")
        || path.contains('{')
        || past_first_page(path)
    {
        return None;
    }
    Some(path)
}

/// Pages past the first stay unconditional: each page's row carries its own
/// generation, so a 304-served page 2 could mix check-runs with a fresh
/// page 1 into one inconsistent assembly. Only a real `page=` query param
/// counts; the `page=` inside `per_page=` must not.
fn past_first_page(path: &str) -> bool {
    let mut rest = path;
    while let Some(i) = rest.find("page=") {
        let is_param = i > 0 && matches!(rest.as_bytes()[i - 1], b'?' | b'&');
        let value = &rest[i + "page=".len()..];
        let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
        if is_param && !digits.is_empty() && digits != "1" {
            return true;
        }
        rest = value;
    }
    false
}

fn row_path(root: &Path, path: &str) -> PathBuf {
    let hex: String = Sha256::digest(path.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    root.join("etag").join(format!("{hex}.json"))
}

struct Row {
    etag: String,
    body: String,
}

fn read_row_at(root: &Path, path: &str, now: f64) -> Option<Row> {
    let file = row_path(root, path);
    let text = fs::read_to_string(&file).ok()?;
    let row: serde_json::Value = serde_json::from_str(&text).ok()?;
    let etag = row.get("etag")?.as_str()?.to_string();
    let body = row.get("body")?.as_str()?.to_string();
    let ts = row.get("ts")?.as_f64()?;
    let expired = ts <= 0.0 || ts - now > CLOCK_SLACK_S || now - ts >= ROW_TTL_S;
    if expired {
        // A parsed stamp past its TTL (or a nonsense one) self-cleans on
        // this touch. An unparseable row leaves before this, so a corrupt
        // file stays for the plain read to bypass.
        drop(fs::remove_file(&file));
        return None;
    }
    Some(Row { etag, body })
}

fn write_row_at(root: &Path, path: &str, etag: &str, body: &str) -> bool {
    let file = row_path(root, path);
    let text = json!({"etag": etag, "body": body, "ts": crate::gh_cache::now_secs()}).to_string();
    if fs::create_dir_all(file.parent().unwrap_or(root)).is_err() {
        return false;
    }
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &file).is_ok()
}

/// Split a `gh api -i` stdout into (status code, header block, body): the
/// status line is the first line, headers end at the first blank line, the
/// rest is the body. gh exits 1 on a 304, so the caller reads the code, not
/// the exit status.
fn split_http(stdout: &str) -> Option<(u16, String, String)> {
    let (head, body) = match stdout.find("\r\n\r\n") {
        Some(i) => (&stdout[..i], &stdout[i + 4..]),
        None => {
            let i = stdout.find("\n\n")?;
            (&stdout[..i], &stdout[i + 2..])
        }
    };
    let status_line = head.lines().next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, head.to_string(), body.to_string()))
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (key, val) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| val.trim().to_string())
    })
}

/// A core response's rate headers land in the fleet ledger: the
/// account-wide remaining and reset ride every conditional response GitHub
/// answers with, so one read keeps the gate current for the whole machine.
/// Every failure is ignored - a missing or unparseable header is no
/// observation, never an error.
fn record_core_from_headers(headers: &str) {
    if header_value(headers, "x-ratelimit-resource").as_deref() != Some("core") {
        return;
    }
    let remaining =
        header_value(headers, "x-ratelimit-remaining").and_then(|v| v.parse::<i64>().ok());
    let reset = header_value(headers, "x-ratelimit-reset").and_then(|v| v.parse::<i64>().ok());
    if let (Some(remaining), Some(reset)) = (remaining, reset) {
        crate::gh_budget::record_core_observation(remaining, reset);
    }
}

/// The one entry the status door calls. Eligible GETs run through
/// `gh api -i` with the stored ETag; everything else is a plain read.
pub(crate) fn conditional_api_get(
    cwd: &Path,
    path: &str,
    spawn: impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String>,
) -> Result<(bool, String, String), String> {
    match crate::gh_cache::rows_root(cwd) {
        Some(root) => conditional_api_get_at(&root, path, &NOT_MODIFIED, spawn),
        None => plain_get(path, spawn),
    }
}

fn plain_get(
    path: &str,
    spawn: impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String>,
) -> Result<(bool, String, String), String> {
    let (ok, stdout, stderr) = spawn_unescaped(&["api", path], &spawn)?;
    Ok((ok, String::from_utf8_lossy(&stdout).into_owned(), stderr))
}

/// Spawn `args` with `--allow-escape-sequences` inserted after `api`, so a
/// response body carrying a literal escape sequence (an annotation message
/// that embeds a CI tool's ANSI color codes, say) is not silently altered
/// by gh's default sanitization before it is cached - a corrupted body
/// cached once replays for the row's whole TTL. Retries once without the
/// flag on an older `gh` that does not know it, exactly as
/// `pr_push::gh_api`'s own unconditional path already falls back.
fn spawn_unescaped(
    args: &[&str],
    spawn: &impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String>,
) -> Result<(bool, Vec<u8>, String), String> {
    let mut with_flag: Vec<&str> = Vec::with_capacity(args.len() + 1);
    with_flag.push(args[0]);
    with_flag.push("--allow-escape-sequences");
    with_flag.extend_from_slice(&args[1..]);
    let (ok, out, err) = spawn(&with_flag)?;
    if !ok && err.to_lowercase().contains("unknown flag") {
        return spawn(args);
    }
    Ok((ok, out, err))
}

/// True when GitHub's response named a further page
/// (`Link: <...>; rel="next"`). Page 1 alone answers the whole resource
/// only when this is false.
fn has_next_link(headers: &str) -> bool {
    header_value(headers, "link")
        .map(|v| v.contains("rel=\"next\""))
        .unwrap_or(false)
}

/// The door with the row store and the 304 counter pinned, so tests point
/// them at a tempdir and a counter of their own: the process-global
/// counter belongs to the spend note, and a test that reads it can race a
/// parallel test's bump.
pub(crate) fn conditional_api_get_at(
    root: &Path,
    path: &str,
    served_304: &AtomicUsize,
    spawn: impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String>,
) -> Result<(bool, String, String), String> {
    let row = read_row_at(root, path, crate::gh_cache::now_secs());
    let mut args: Vec<&str> = vec!["api", "-i", path];
    let cond;
    if let Some(row) = &row {
        // The stored value goes out verbatim; the probe showed a weak
        // W/"..." ETag returns 304.
        cond = format!("If-None-Match: {}", row.etag);
        args.push("-H");
        args.push(&cond);
    }
    let (ok, stdout_bytes, stderr) = spawn_unescaped(&args, &spawn)?;
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let Some((status, headers, body)) = split_http(&stdout) else {
        return Ok((ok, stdout, stderr));
    };
    record_core_from_headers(&headers);
    match status {
        304 => match row {
            Some(row) => {
                served_304.fetch_add(1, Ordering::SeqCst);
                Ok((true, row.body, String::new()))
            }
            None => Ok((ok, body, stderr)),
        },
        200 => {
            // A failed transfer can still carry a 200 envelope with a
            // truncated body (reset mid-read). Caching it would pin the
            // bad body behind a 304 for the row's whole TTL, so a
            // non-zero exit caches nothing and keeps the caller's error.
            if !ok {
                return Ok((ok, body, stderr));
            }
            if let Some(etag) = header_value(&headers, "etag") {
                write_row_at(root, path, &etag, &body);
            }
            Ok((true, body, stderr))
        }
        _ => Ok((ok, body, stderr)),
    }
}

/// Like [`conditional_api_get`], but the resource is a caller-assembled
/// multi-page read (check-runs, annotations, an open-pulls listing): the
/// probe conditions on page 1's ETag. GitHub ETags each page
/// independently (its own REST API guidance: a page's representation, and
/// so its ETag, can change without any other page changing), so a 304 on
/// page 1 proves only that page 1 is unchanged - it is never, by itself,
/// proof that a LATER page is. This door therefore only ever caches (and
/// so only ever trusts a future 304 for) a resource it confirmed was a
/// single page at write time: when GitHub names no further page, the
/// probe's own page-1 body already IS the whole answer, no `fetch_all`
/// needed - the common case for check-runs/annotations/open-pulls, which
/// almost never exceed one page, so a resource that changes on nearly
/// every poll never costs more than the single unconditional call this
/// replaces. A genuinely multi-page answer always pays `fetch_all` (the
/// caller's real paginated fetch) and is never cached, so every later
/// read for that path probes unconditionally (no stored ETag to send) and
/// pays the same full fetch every time - exactly today's cost, never
/// less, but never a stale answer either.
pub(crate) fn conditional_paginated_get(
    cwd: &Path,
    path: &str,
    fetch_all: impl Fn() -> Result<String, String>,
    spawn: impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String>,
) -> Result<(bool, String, String), String> {
    match crate::gh_cache::rows_root(cwd) {
        Some(root) => conditional_paginated_get_at(&root, path, &NOT_MODIFIED, fetch_all, spawn),
        None => Ok((true, fetch_all()?, String::new())),
    }
}

/// The row-store-and-counter-pinned form of [`conditional_paginated_get`],
/// for tests - the same split [`conditional_api_get_at`] has.
pub(crate) fn conditional_paginated_get_at(
    root: &Path,
    path: &str,
    served_304: &AtomicUsize,
    fetch_all: impl Fn() -> Result<String, String>,
    spawn: impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String>,
) -> Result<(bool, String, String), String> {
    let row = read_row_at(root, path, crate::gh_cache::now_secs());
    let mut args: Vec<&str> = vec!["api", "-i", path];
    let cond;
    if let Some(row) = &row {
        cond = format!("If-None-Match: {}", row.etag);
        args.push("-H");
        args.push(&cond);
    }
    let (ok, stdout_bytes, stderr) = spawn_unescaped(&args, &spawn)?;
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let Some((status, headers, probe_body)) = split_http(&stdout) else {
        // An unparseable probe answers nothing about change; the real
        // fetch_all is the only thing that can still answer truthfully.
        // It just ran successfully (its `?` would have returned on
        // failure), so this is a real success, never the probe's stale
        // failure status.
        return Ok((true, fetch_all()?, stderr));
    };
    record_core_from_headers(&headers);
    if status == 304 {
        if let Some(row) = row {
            served_304.fetch_add(1, Ordering::SeqCst);
            return Ok((true, row.body, String::new()));
        }
        // Unreachable in practice: GitHub only answers 304 to a request
        // that carried an If-None-Match, which this probe only sends when
        // `row` is `Some`. Falls through rather than inventing a body.
    }
    if !ok {
        // The probe itself already reports a failed transfer; a failure
        // never caches and never pays a second read to confirm it.
        return Ok((false, String::new(), stderr));
    }
    if has_next_link(&headers) {
        // Genuinely multi-page: page 1's ETag vouches only for page 1
        // (GitHub ETags each page independently), never for page 2+, so
        // this answer is never cached - a cached row would let a later
        // 304 on page 1 alone replay a stale assembly whose later pages
        // may have changed on their own. `row` (if any) is left exactly
        // as it was; it can only have been written by the single-page
        // branch below, so trusting it there stays sound.
        return Ok((true, fetch_all()?, stderr));
    }
    let body = match serde_json::from_str::<Value>(&probe_body) {
        Ok(page1) => serde_json::to_string(&Value::Array(vec![page1]))
            .map_err(|e| format!("gh api {path} page unserializable: {e}"))?,
        // An unparseable 200 body is the same shape as a truncated
        // transfer the `!ok` check above did not catch; fetch_all is the
        // only thing left that can still answer truthfully. Also never
        // cached: a body this call could not even parse names nothing to
        // seed the row with.
        Err(_) => return Ok((true, fetch_all()?, stderr)),
    };
    if let Some(etag) = header_value(&headers, "etag") {
        write_row_at(root, path, &etag, &body);
    }
    Ok((true, body, stderr))
}

/// The spend-note suffix: `, {n} served 304` once the process has served a
/// 304, empty before, so callers that parse the note never see a new shape
/// on the plain path.
pub(crate) fn note_suffix() -> String {
    let n = NOT_MODIFIED.load(Ordering::SeqCst);
    if n > 0 {
        format!(", {n} served 304")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_of(
        stdout: &str,
        exit_ok: bool,
    ) -> impl Fn(&[&str]) -> Result<(bool, Vec<u8>, String), String> {
        let stdout = stdout.to_string();
        move |_args| Ok((exit_ok, stdout.clone().into_bytes(), String::new()))
    }

    #[test]
    fn conditional_serves_row_on_304() {
        let root = tempfile::tempdir().unwrap();
        write_row_at(root.path(), "repos/o/r/pulls/1", "W/\"abc\"", "{\"n\":1}");
        let counter = AtomicUsize::new(0);
        let spawn = spawn_of(
            "HTTP/2.0 304 Not Modified\r\netag: W/\"abc\"\r\n\r\n",
            false,
        );
        let (ok, body, stderr) =
            conditional_api_get_at(root.path(), "repos/o/r/pulls/1", &counter, spawn).unwrap();
        assert!(ok);
        assert_eq!(body, "{\"n\":1}");
        assert_eq!(stderr, "");
        // The test's own counter sees exactly this call's serve, so a
        // parallel test on the process-global counter cannot touch it.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn future_ts_row_is_a_miss() {
        let root = tempfile::tempdir().unwrap();
        let file = row_path(root.path(), "repos/o/r/pulls/4");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let future = crate::gh_cache::now_secs() + 3600.0;
        fs::write(
            &file,
            json!({"etag": "W/\"fw\"", "body": "{\"old\":1}", "ts": future}).to_string(),
        )
        .unwrap();
        let now = crate::gh_cache::now_secs();
        assert!(read_row_at(root.path(), "repos/o/r/pulls/4", now).is_none());
        // An expired row with a parsed stamp self-cleans on its next touch.
        assert!(!file.exists());
    }

    #[test]
    fn failed_200_transfer_caches_nothing() {
        let root = tempfile::tempdir().unwrap();
        let counter = AtomicUsize::new(0);
        // Headers arrived; the body read was cut short and gh exited 1.
        let spawn = spawn_of("HTTP/2.0 200 OK\r\nETag: W/\"cut\"\r\n\r\n{\"trunc", false);
        let (ok, body, _) =
            conditional_api_get_at(root.path(), "repos/o/r/pulls/5", &counter, spawn).unwrap();
        assert!(!ok);
        assert_eq!(body, "{\"trunc");
        let now = crate::gh_cache::now_secs();
        assert!(read_row_at(root.path(), "repos/o/r/pulls/5", now).is_none());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn first_200_seeds_row_and_returns_body() {
        let root = tempfile::tempdir().unwrap();
        let stdout = "HTTP/2.0 200 OK\r\ncontent-type: application/json\r\nEtag: W/\"zz\"\r\n\r\n{\"y\":2}\n";
        let spawn = spawn_of(stdout, true);
        let counter = AtomicUsize::new(0);
        let (ok, body, _) =
            conditional_api_get_at(root.path(), "repos/o/r/pulls/2", &counter, spawn).unwrap();
        assert!(ok);
        assert_eq!(body, "{\"y\":2}\n");
        let now = crate::gh_cache::now_secs();
        let row = read_row_at(root.path(), "repos/o/r/pulls/2", now).expect("row written");
        assert_eq!(row.etag, "W/\"zz\"");
        assert_eq!(row.body, "{\"y\":2}\n");
    }

    #[test]
    fn budget_refusal_propagates_and_writes_no_row() {
        let root = tempfile::tempdir().unwrap();
        write_row_at(root.path(), "repos/o/r/pulls/3", "W/\"old\"", "{\"n\":0}");
        let counter = AtomicUsize::new(0);
        let spawn = |_args: &[&str]| -> Result<(bool, Vec<u8>, String), String> {
            Err("pr-status: refused by the fleet request budget".to_string())
        };
        let err =
            conditional_api_get_at(root.path(), "repos/o/r/pulls/3", &counter, spawn).unwrap_err();
        assert!(err.contains("refused"));
        let now = crate::gh_cache::now_secs();
        let row = read_row_at(root.path(), "repos/o/r/pulls/3", now).expect("row untouched");
        assert_eq!(row.etag, "W/\"old\"");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn eligible_rejects_writes_pages_and_logs() {
        let api = "api".to_string();
        let plain = vec![api.clone(), "repos/o/r/pulls/1".to_string()];
        assert_eq!(eligible(&plain), Some("repos/o/r/pulls/1"));
        let write = vec![
            api.clone(),
            "-X".to_string(),
            "POST".to_string(),
            "repos/o/r/pulls/1".to_string(),
        ];
        assert_eq!(eligible(&write), None);
        let page2 = vec![
            api.clone(),
            "repos/o/r/commits/sha/check-runs?per_page=100&page=2".to_string(),
        ];
        assert_eq!(eligible(&page2), None);
        let page1 = vec![
            api.clone(),
            "repos/o/r/commits/sha/check-runs?per_page=100&page=1".to_string(),
        ];
        assert_eq!(
            eligible(&page1),
            Some("repos/o/r/commits/sha/check-runs?per_page=100&page=1")
        );
        let logs = vec![api.clone(), "repos/o/r/actions/jobs/9/logs".to_string()];
        assert_eq!(eligible(&logs), None);
        let placeholder = vec![
            api.clone(),
            "repos/{owner}/{repo}/rules/branches/main".to_string(),
        ];
        assert_eq!(eligible(&placeholder), None);
        let not_repos = vec![api, "graphql".to_string()];
        assert_eq!(eligible(&not_repos), None);
    }

    #[test]
    fn paginated_304_replays_the_full_cached_assembly_and_never_calls_fetch_all() {
        let root = tempfile::tempdir().unwrap();
        write_row_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            "W/\"abc\"",
            "[{\"a\":1},{\"a\":2}]",
        );
        let counter = AtomicUsize::new(0);
        let spawn = spawn_of(
            "HTTP/2.0 304 Not Modified\r\netag: W/\"abc\"\r\n\r\n",
            false,
        );
        let fetch_all = || -> Result<String, String> { panic!("fetch_all must not run on a 304") };
        let (ok, body, _) = conditional_paginated_get_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            &counter,
            fetch_all,
            spawn,
        )
        .unwrap();
        assert!(ok);
        assert_eq!(body, "[{\"a\":1},{\"a\":2}]");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn paginated_200_with_no_next_link_answers_from_page_1_alone_and_never_calls_fetch_all() {
        let root = tempfile::tempdir().unwrap();
        write_row_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            "W/\"old\"",
            "[{\"a\":1}]",
        );
        let counter = AtomicUsize::new(0);
        let spawn = spawn_of(
            "HTTP/2.0 200 OK\r\netag: W/\"new\"\r\n\r\n{\"check_runs\":[{\"a\":2}]}",
            true,
        );
        let fetch_all =
            || -> Result<String, String> { panic!("fetch_all must not run with no next link") };
        let (ok, body, _) = conditional_paginated_get_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            &counter,
            fetch_all,
            spawn,
        )
        .unwrap();
        assert!(ok);
        assert_eq!(body, "[{\"check_runs\":[{\"a\":2}]}]");
        let now = crate::gh_cache::now_secs();
        let row = read_row_at(root.path(), "repos/o/r/commits/sha/check-runs", now)
            .expect("row recached");
        assert_eq!(row.etag, "W/\"new\"");
        assert_eq!(row.body, "[{\"check_runs\":[{\"a\":2}]}]");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn paginated_200_with_a_next_link_refetches_everything_and_is_never_cached() {
        // A genuinely multi-page resource: page 1's ETag vouches only for
        // page 1 (GitHub ETags each page independently), so a cached row
        // here would let a later 304-on-page-1 replay a stale assembly
        // whose page 2 changed on its own - the exact risk a reviewer
        // named. The pre-existing row must therefore survive UNCHANGED,
        // never overwritten with this read's fresh etag/body.
        let root = tempfile::tempdir().unwrap();
        write_row_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            "W/\"old\"",
            "[{\"a\":1}]",
        );
        let counter = AtomicUsize::new(0);
        let spawn = spawn_of(
            "HTTP/2.0 200 OK\r\netag: W/\"new\"\r\nLink: <https://api.github.com/x?page=2>; rel=\"next\"\r\n\r\n{\"check_runs\":[]}",
            true,
        );
        let fetch_all =
            || -> Result<String, String> { Ok("[{\"a\":1},{\"a\":2},{\"a\":3}]".to_string()) };
        let (ok, body, _) = conditional_paginated_get_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            &counter,
            fetch_all,
            spawn,
        )
        .unwrap();
        assert!(ok);
        assert_eq!(body, "[{\"a\":1},{\"a\":2},{\"a\":3}]");
        let now = crate::gh_cache::now_secs();
        let row = read_row_at(root.path(), "repos/o/r/commits/sha/check-runs", now)
            .expect("the pre-existing row must survive, untouched");
        assert_eq!(row.etag, "W/\"old\"");
        assert_eq!(row.body, "[{\"a\":1}]");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn paginated_unparseable_probe_still_reports_a_successful_fallback_fetch() {
        // A probe response gh could not even frame as HTTP (a transient
        // connection failure before headers arrive) must not leak its
        // stale failure status past a fetch_all that then succeeds -
        // reviewer finding: the caller must not discard a good body
        // because of an unrelated probe failure.
        let root = tempfile::tempdir().unwrap();
        let counter = AtomicUsize::new(0);
        let spawn = |_args: &[&str]| -> Result<(bool, Vec<u8>, String), String> {
            Ok((
                false,
                b"not http at all".to_vec(),
                "connection reset".to_string(),
            ))
        };
        let fetch_all = || -> Result<String, String> { Ok("[{\"a\":1},{\"a\":2}]".to_string()) };
        let (ok, body, _) = conditional_paginated_get_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            &counter,
            fetch_all,
            spawn,
        )
        .unwrap();
        assert!(ok);
        assert_eq!(body, "[{\"a\":1},{\"a\":2}]");
    }

    #[test]
    fn paginated_failed_probe_never_calls_fetch_all_and_caches_nothing() {
        let root = tempfile::tempdir().unwrap();
        let counter = AtomicUsize::new(0);
        let spawn = spawn_of("HTTP/2.0 200 OK\r\netag: W/\"zz\"\r\n\r\n{\"trunc", false);
        let fetch_all = || -> Result<String, String> {
            panic!("fetch_all must not run after a failed transfer")
        };
        let (ok, body, _) = conditional_paginated_get_at(
            root.path(),
            "repos/o/r/commits/sha/check-runs",
            &counter,
            fetch_all,
            spawn,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(body, "");
        let now = crate::gh_cache::now_secs();
        assert!(read_row_at(root.path(), "repos/o/r/commits/sha/check-runs", now).is_none());
    }
}

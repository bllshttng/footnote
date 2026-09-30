//! Bounded import of fno and Ghostty theme files into the user's theme folder.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncReadExt;

use crate::keys::KeymapWarning;
use crate::theme::Theme;

const MAX_BYTES: usize = 65_536;
const MAX_FOLDER_FILES: usize = 32;
const MAX_NAME_LEN: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    File(PathBuf),
    Folder(PathBuf),
    Url(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) name: String,
    pub(crate) rename_reason: Option<String>,
    pub(crate) theme: Theme,
    pub(crate) spec: Vec<(String, String)>,
    pub(crate) warnings: Vec<KeymapWarning>,
}

#[derive(Debug, Default)]
pub(crate) struct Preview {
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) skipped: Vec<String>,
}

enum InputFile {
    One(PathBuf),
    Many(Vec<PathBuf>),
    Url(String),
}

pub(crate) fn parse_source(input: &str, cwd: &Path) -> Result<Source, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter a theme file path, folder path, or public GitHub file URL.".into());
    }
    if has_url_scheme(input)
        || starts_with_ascii_case(input, "github.com/")
        || starts_with_ascii_case(input, "www.github.com/")
        || starts_with_ascii_case(input, "raw.githubusercontent.com/")
    {
        return canonical_github_url(input).map(Source::Url);
    }
    let path = expand_path(input, cwd)?;
    let metadata = fs::metadata(&path).map_err(|e| path_error(&path, &e))?;
    if metadata.is_dir() {
        return Ok(Source::Folder(path));
    }
    if !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file or folder.",
            path.display()
        ));
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err(format!("{} is larger than 65,536 bytes.", path.display()));
    }
    Ok(Source::File(path))
}

fn has_url_scheme(input: &str) -> bool {
    let Some((scheme, _)) = input.split_once("://") else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
}

fn starts_with_ascii_case(input: &str, prefix: &str) -> bool {
    input
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn expand_path(input: &str, cwd: &Path) -> Result<PathBuf, String> {
    let expanded = if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or("HOME is not set; use an absolute path.")?;
        PathBuf::from(home).join(input.strip_prefix("~/").unwrap_or(""))
    } else {
        PathBuf::from(input)
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    })
}

fn canonical_github_url(input: &str) -> Result<String, String> {
    let input = input.trim();
    let with_scheme = if input
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        format!("https://{}", &input[8..])
    } else if input
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        format!("http://{}", &input[7..])
    } else if input
        .get(..11)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("github.com/"))
    {
        format!("https://{input}")
    } else {
        input.to_string()
    };
    let without_fragment = with_scheme.split('#').next().unwrap_or_default();
    if without_fragment.starts_with("http://") {
        return Err(
            "GitHub theme URLs must use HTTPS. Paste an https://github.com/.../blob/... file URL."
                .into(),
        );
    }
    if without_fragment.contains('?') {
        return Err("GitHub theme URLs cannot include a query string. Paste the public file URL without query parameters.".into());
    }
    let Some(rest) = without_fragment.strip_prefix("https://") else {
        return Err(
            "Use an https://github.com/... or https://raw.githubusercontent.com/... file URL."
                .into(),
        );
    };
    let (authority, path) = rest.split_once('/').ok_or_else(file_url_hint)?;
    if authority.contains('@') {
        return Err(
            "GitHub theme URLs cannot include credentials. Paste a public GitHub file URL.".into(),
        );
    }
    if authority.contains(':') {
        return Err(
            "GitHub theme URLs cannot use an explicit port. Paste the standard HTTPS file URL."
                .into(),
        );
    }
    let host = authority.to_ascii_lowercase();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if host == "github.com" {
        if parts.len() >= 5 && (parts[2] == "blob" || parts[2] == "raw") {
            let file_path = parts[4..].join("/");
            if parts[0].is_empty()
                || parts[1].is_empty()
                || parts[3].is_empty()
                || file_path.is_empty()
            {
                return Err(file_url_hint());
            }
            return Ok(format!(
                "https://raw.githubusercontent.com/{}/{}/{}/{}",
                parts[0], parts[1], parts[3], file_path
            ));
        }
        return Err(file_url_hint());
    }
    if host == "raw.githubusercontent.com" && parts.len() >= 4 {
        return Ok(format!(
            "https://raw.githubusercontent.com/{}/{}/{}/{}",
            parts[0],
            parts[1],
            parts[2],
            parts[3..].join("/")
        ));
    }
    Err("Only public files on github.com and raw.githubusercontent.com are supported.".into())
}

fn file_url_hint() -> String {
    "Paste one theme file URL: https://github.com/<owner>/<repo>/blob/<branch>/<path>".into()
}

fn path_error(path: &Path, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("Theme path {} was not found.", path.display())
    } else {
        format!("Cannot read theme path {}: {error}.", path.display())
    }
}

#[cfg(test)]
pub(crate) async fn preview(source: &Source, cwd: &Path) -> Result<Preview, String> {
    preview_with_theme_dir(source, cwd, crate::digest_overlay::themes_dir()).await
}

pub(crate) async fn preview_with_theme_dir(
    source: &Source,
    cwd: &Path,
    theme_dir: Option<PathBuf>,
) -> Result<Preview, String> {
    let input = match source {
        Source::File(path) => InputFile::One(path.clone()),
        Source::Folder(path) => InputFile::Many(folder_files(path)?),
        Source::Url(url) => InputFile::Url(url.clone()),
    };
    let (files, mut skipped) = match input {
        InputFile::One(path) => (
            vec![(path.display().to_string(), read_theme_file(&path)?)],
            Vec::new(),
        ),
        InputFile::Many(paths) => {
            let mut files = Vec::new();
            let mut skipped = Vec::new();
            for path in paths {
                match read_theme_file(&path) {
                    Ok(text) => files.push((path.display().to_string(), text)),
                    Err(reason) => skipped.push(format!(
                        "{}: {reason}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )),
                }
            }
            (files, skipped)
        }
        InputFile::Url(url) => {
            let bytes = fetch(&url).await?;
            let text = String::from_utf8(bytes)
                .map_err(|_| "The GitHub theme file is not valid UTF-8.".to_string())?;
            (vec![(url, text)], Vec::new())
        }
    };

    let mut names = taken_names(cwd, theme_dir.as_deref());
    let mut candidates = Vec::new();
    for (source_name, text) in files {
        let stem = source_stem(&source_name);
        let parsed = match parse_theme_text(&text, &stem) {
            Ok(parsed) => parsed,
            Err(reason)
                if matches!(source, Source::Folder(_))
                    && reason == "not an fno or Ghostty theme file" =>
            {
                skipped.push(format!(
                    "{}: not a theme file",
                    Path::new(&source_name)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ));
                continue;
            }
            Err(reason) => return Err(reason),
        };
        for (raw_name, spec) in parsed {
            let item = candidate(&raw_name, spec, &names)?;
            names.insert(item.name.clone());
            candidates.push(item);
        }
    }
    Ok(Preview {
        candidates,
        skipped,
    })
}

fn source_stem(source_name: &str) -> String {
    let filename = if source_name.starts_with("https://") {
        let encoded = source_name.rsplit('/').next().unwrap_or("theme");
        percent_decode_component(encoded).unwrap_or_else(|| encoded.to_string())
    } else {
        source_name.to_string()
    };
    Path::new(&filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("theme")
        .to_string()
}

fn percent_decode_component(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let high = hex_nibble(bytes[at + 1])?;
            let low = hex_nibble(bytes[at + 2])?;
            decoded.push((high << 4) | low);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn folder_files(folder: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = fs::metadata(folder).map_err(|e| path_error(folder, &e))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a folder.", folder.display()));
    }
    let entries = fs::read_dir(folder).map_err(|e| path_error(folder, &e))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("Cannot read a file in {}: {e}.", folder.display()))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let metadata = fs::metadata(entry.path()).map_err(|e| path_error(&entry.path(), &e))?;
        if metadata.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    if files.len() > MAX_FOLDER_FILES {
        return Err(format!(
            "This folder has {} files; name one file or use a folder with no more than 32 files.",
            files.len()
        ));
    }
    Ok(files)
}

fn read_theme_file(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|e| path_error(path, &e))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file.", path.display()));
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err(format!("{} is larger than 65,536 bytes.", path.display()));
    }
    let mut file = fs::File::open(path).map_err(|e| path_error(path, &e))?;
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Cannot read {}: {e}.", path.display()))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{} is larger than 65,536 bytes.", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8.", path.display()))
}

fn taken_names(cwd: &Path, theme_dir: Option<&Path>) -> HashSet<String> {
    let mut names = crate::theme::THEME_NAMES
        .iter()
        .map(|name| name.to_string())
        .collect::<HashSet<_>>();
    names.extend(
        crate::digest_overlay::user_themes(cwd)
            .0
            .into_iter()
            .map(|(name, _)| name.to_ascii_lowercase()),
    );
    if let Some(dir) = theme_dir {
        if let Ok(entries) = fs::read_dir(dir) {
            names.extend(entries.flatten().filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_ascii_lowercase)
            }));
        }
    }
    names
}

pub(crate) fn parse_theme_text(
    text: &str,
    stem: &str,
) -> Result<Vec<(String, Vec<(String, String)>)>, String> {
    if let Ok(table) = text.parse::<toml::Table>() {
        let themes = crate::digest_overlay::themes_from_str(text);
        if !themes.is_empty() {
            return Ok(themes);
        }
        let roles = [
            "inherit",
            "base",
            "stamp",
            "border",
            "title",
            "brand",
            "needs_you",
            "sel",
            "dim",
            "chip",
        ];
        if roles.iter().any(|key| table.contains_key(*key)) {
            let spec = table
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| value.to_string()),
                    )
                })
                .collect::<Vec<_>>();
            return Ok(vec![(stem.to_string(), spec)]);
        }
    }
    parse_ghostty(text, stem).ok_or_else(|| "not an fno or Ghostty theme file".to_string())
}

fn parse_ghostty(text: &str, stem: &str) -> Option<Vec<(String, Vec<(String, String)>)>> {
    let mut keys = BTreeMap::<String, String>::new();
    let mut palette = BTreeMap::<u8, String>::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key {
            "background" | "foreground" | "selection-background" => {
                keys.insert(key.to_string(), ghostty_color(value));
            }
            "palette" => {
                if let Some((slot, color)) = value.split_once('=') {
                    if let Ok(slot) = slot.trim().parse::<u8>() {
                        if slot <= 15 {
                            palette.insert(slot, ghostty_color(color.trim()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if keys.is_empty() && palette.is_empty() {
        return None;
    }
    let mut spec = Vec::new();
    let mut push = |role: &str, value: Option<&String>| {
        if let Some(value) = value {
            spec.push((role.to_string(), value.clone()));
        }
    };
    push("base", keys.get("background"));
    push("stamp", keys.get("foreground"));
    push("title", keys.get("foreground"));
    push("brand", palette.get(&4));
    push("border", palette.get(&4));
    push("needs_you", palette.get(&3));
    push("chip", palette.get(&1));
    push(
        "sel",
        keys.get("selection-background").or_else(|| palette.get(&0)),
    );
    push("dim", palette.get(&8));
    let inherit = keys
        .get("background")
        .and_then(|color| color_luminance(color))
        .map(|luminance| {
            if luminance > 0.5 {
                "footnote-paper"
            } else {
                "footnote-superscript"
            }
        })
        .unwrap_or("footnote-superscript");
    spec.push(("inherit".to_string(), inherit.to_string()));
    Some(vec![(stem.to_string(), spec)])
}

fn ghostty_color(value: &str) -> String {
    let value = value.trim();
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        format!("#{hex}")
    } else {
        value.to_string()
    }
}

fn color_luminance(value: &str) -> Option<f64> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f64 / 255.0;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f64 / 255.0;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f64 / 255.0;
    let linear = |channel: f64| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

pub(crate) fn final_name(raw: &str, taken: &HashSet<String>) -> Result<String, String> {
    let mut normalized = String::new();
    let mut dash = false;
    for character in raw.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            normalized.push(character);
            dash = false;
        } else if !normalized.is_empty() && !dash {
            normalized.push('-');
            dash = true;
        }
        if normalized.len() >= MAX_NAME_LEN {
            break;
        }
    }
    let normalized = normalized.trim_matches('-').to_string();
    if !normalized.bytes().any(|byte| byte.is_ascii_alphanumeric()) {
        return Err("The theme name must contain an ASCII letter or number.".into());
    }
    for suffix in 1..=99 {
        let candidate = if suffix == 1 {
            normalized.clone()
        } else {
            let tail = format!("-{suffix}");
            format!(
                "{}{}",
                normalized
                    .chars()
                    .take(MAX_NAME_LEN - tail.len())
                    .collect::<String>()
                    .trim_end_matches('-'),
                tail
            )
        };
        if !taken
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&candidate))
        {
            return Ok(candidate);
        }
    }
    Err(format!(
        "No free name remains for theme {normalized} (suffix limit 99)."
    ))
}

pub(crate) fn candidate(
    raw_name: &str,
    spec: Vec<(String, String)>,
    taken: &HashSet<String>,
) -> Result<Candidate, String> {
    let normalized = final_name(raw_name, &HashSet::new())?;
    let name = final_name(raw_name, taken)?;
    let builtin = crate::theme::THEME_NAMES
        .iter()
        .any(|builtin| builtin.eq_ignore_ascii_case(&normalized));
    let collided = taken
        .iter()
        .any(|held| held.eq_ignore_ascii_case(&normalized));
    let rename_reason = (name != raw_name).then(|| {
        if builtin {
            format!("{raw_name} is a shipped theme")
        } else if collided {
            format!("{raw_name} is already your theme")
        } else {
            format!("{raw_name} was normalized for a file name")
        }
    });
    let (theme, warnings) = crate::digest_overlay::materialize_user_theme(&name, &spec);
    Ok(Candidate {
        name,
        rename_reason,
        theme,
        spec,
        warnings,
    })
}

pub(crate) async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    fetch_with(Path::new("curl"), url).await
}

async fn fetch_with(program: &Path, url: &str) -> Result<Vec<u8>, String> {
    let canonical = canonical_github_url(url)?;
    let mut child = tokio::process::Command::new(program)
        .args([
            "-q",
            "-sS",
            "-f",
            "-L",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "3",
            "--max-time",
            "10",
            "--max-filesize",
            "65536",
            &canonical,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Cannot start curl to fetch the public GitHub theme file.".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("curl did not provide a response body.")?;
    let mut bytes = Vec::with_capacity(MAX_BYTES + 1);
    let mut chunk = [0u8; 8192];
    loop {
        let capacity = (MAX_BYTES + 1 - bytes.len()).min(chunk.len());
        let count = stdout
            .read(&mut chunk[..capacity])
            .await
            .map_err(|_| "Could not read the GitHub theme response.".to_string())?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > MAX_BYTES {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("The GitHub theme file is larger than 65,536 bytes.".into());
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|_| "Could not finish the GitHub theme fetch.".to_string())?;
    if !status.success() {
        return Err(match status.code() {
            Some(6) => "GitHub host could not be resolved.".into(),
            Some(22) => "GitHub did not return a public theme file.".into(),
            Some(28) => "The GitHub theme fetch timed out.".into(),
            Some(63) => "The GitHub theme file is larger than 65,536 bytes.".into(),
            _ => "The GitHub theme fetch failed.".into(),
        });
    }
    std::str::from_utf8(&bytes)
        .map_err(|_| "The GitHub theme file is not valid UTF-8.".to_string())?;
    Ok(bytes)
}

pub(crate) fn render_file(
    name: &str,
    spec: &[(String, String)],
    source: &str,
    date: &str,
) -> String {
    let source = source.replace('\n', " ").replace('\r', " ");
    let date = date.replace('\n', " ").replace('\r', " ");
    let mut output = format!("# Imported from {source} on {date}.\n[mux.themes.{name}]\n");
    for key in [
        "inherit",
        "base",
        "stamp",
        "border",
        "title",
        "brand",
        "needs_you",
        "sel",
        "dim",
        "chip",
    ] {
        if let Some((_, value)) = spec.iter().find(|(candidate, _)| candidate == key) {
            output.push_str(key);
            output.push_str(" = \"");
            output.push_str(&value.replace('\\', "\\\\").replace('"', "\\\""));
            output.push_str("\"\n");
        }
    }
    output
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn save_all(
    dir: &Path,
    candidates: &[Candidate],
    source: &str,
    date: &str,
) -> Result<Vec<String>, String> {
    if candidates
        .iter()
        .any(|candidate| !candidate.warnings.is_empty())
    {
        return Err("Fix every theme warning before saving.".into());
    }
    fs::create_dir_all(dir).map_err(|e| format!("Cannot create the theme folder: {e}."))?;
    let mut saved = Vec::<PathBuf>::new();
    let mut names = Vec::new();
    let mut taken = HashSet::new();
    for candidate in candidates {
        let mut name = candidate.name.clone();
        loop {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temp = dir.join(format!(".theme-{}-{id}.tmp", std::process::id()));
            let target = dir.join(format!("{name}.toml"));
            let rendered = render_file(&name, &candidate.spec, source, date);
            let write_result = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .and_then(|mut file| {
                    file.write_all(rendered.as_bytes())?;
                    file.sync_all()
                });
            if let Err(error) = write_result {
                let _ = fs::remove_file(&temp);
                rollback(&saved);
                return Err(format!("Cannot stage theme {name}: {error}."));
            }
            match fs::hard_link(&temp, &target) {
                Ok(()) => {
                    let _ = fs::remove_file(&temp);
                    saved.push(target);
                    names.push(name.clone());
                    taken.insert(name.clone());
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temp);
                    let mut occupied = taken.clone();
                    occupied.insert(name.clone());
                    match final_name(&candidate.name, &occupied) {
                        Ok(next) => name = next,
                        Err(reason) => {
                            rollback(&saved);
                            return Err(reason);
                        }
                    }
                }
                Err(error) => {
                    let _ = fs::remove_file(&temp);
                    rollback(&saved);
                    return Err(format!("Cannot save theme {name}: {error}."));
                }
            }
        }
    }
    Ok(names)
}

fn rollback(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    async fn assert_local_and_github_sources_are_bounded_and_canonical() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("theme.toml"), "base = \"#112233\"").unwrap();
        assert_eq!(
            parse_source("theme.toml", root.path()),
            Ok(Source::File(root.path().join("theme.toml")))
        );
        assert_eq!(
            parse_source(
                "https://github.com/octo/themes/blob/main/mocha.conf#preview",
                root.path()
            ),
            Ok(Source::Url(
                "https://raw.githubusercontent.com/octo/themes/main/mocha.conf".into()
            ))
        );
        assert_eq!(
            parse_source("github.com/octo/themes/raw/main/mocha.conf", root.path()),
            Ok(Source::Url(
                "https://raw.githubusercontent.com/octo/themes/main/mocha.conf".into()
            ))
        );
        assert_eq!(
            parse_source(
                "https://raw.githubusercontent.com/octo/themes/main/mocha.conf",
                root.path()
            ),
            Ok(Source::Url(
                "https://raw.githubusercontent.com/octo/themes/main/mocha.conf".into()
            ))
        );
        for (url, reason) in [
            (
                "http://github.com/octo/themes/blob/main/mocha.conf",
                "HTTPS",
            ),
            (
                "HTTP://github.com/octo/themes/blob/main/mocha.conf",
                "HTTPS",
            ),
            (
                "HTTPS://github.com/octo/themes/blob/main/mocha.conf?token=secret",
                "query",
            ),
            (
                "ftp://github.com/octo/themes/blob/main/mocha.conf?token=secret",
                "query",
            ),
            (
                "raw.githubusercontent.com/octo/themes/main/mocha.conf?token=secret",
                "query",
            ),
            (
                "www.github.com/octo/themes/blob/main/mocha.conf?token=secret",
                "query",
            ),
            (
                "https://evil.test/octo/themes/blob/main/mocha.conf",
                "Only public files",
            ),
            (
                "https://user@github.com/octo/themes/blob/main/mocha.conf",
                "credentials",
            ),
            (
                "https://github.com:8443/octo/themes/blob/main/mocha.conf",
                "port",
            ),
            (
                "https://github.com/octo/themes/blob/main/mocha.conf?token=secret",
                "query",
            ),
            ("https://github.com/octo/themes/tree/main", "/blob/"),
            ("https://github.com/octo/themes", "/blob/"),
        ] {
            assert!(
                parse_source(url, root.path()).unwrap_err().contains(reason),
                "refusal should name {reason}"
            );
        }
        let too_large = root.path().join("large.toml");
        fs::write(&too_large, vec![b'x'; 65_537]).unwrap();
        assert!(parse_source("large.toml", root.path())
            .unwrap_err()
            .contains("65,536"));
        assert!(parse_source("missing.toml", root.path())
            .unwrap_err()
            .contains("not found"));
        fs::write(root.path().join("binary.toml"), [0xff, 0xfe]).unwrap();
        let binary = parse_source("binary.toml", root.path()).unwrap();
        assert!(preview(&binary, root.path())
            .await
            .unwrap_err()
            .contains("UTF-8"));
        #[cfg(unix)]
        {
            let socket_path = root.path().join("not-a-file.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
            assert!(parse_source("not-a-file.sock", root.path())
                .unwrap_err()
                .contains("not a regular file or folder"));
        }
    }

    #[test]
    fn fno_and_ghostty_text_build_named_color_candidates() {
        let fno = parse_theme_text(
            "[mux.themes.midnight]\nbase = \"#101018\"\nbrand = \"blue\"",
            "ignored",
        )
        .unwrap();
        assert_eq!(fno.len(), 1);
        assert_eq!(fno[0].0, "midnight");
        assert_eq!(
            source_stem("https://raw.githubusercontent.com/o/r/main/Catppuccin%20Mocha.conf"),
            "Catppuccin Mocha"
        );
        assert_eq!(
            source_stem("/tmp/Catppuccin Mocha.conf"),
            "Catppuccin Mocha"
        );
        let top = parse_theme_text("base = \"#101018\"\nbrand = \"blue\"", "top-level").unwrap();
        assert_eq!(top[0].0, "top-level");
        let top_unknown = parse_theme_text("base = \"#101018\"\nunknown = [\"red\"]", "top-level")
            .unwrap()
            .remove(0);
        assert_eq!(
            candidate(&top_unknown.0, top_unknown.1, &HashSet::new())
                .unwrap()
                .warnings
                .len(),
            1
        );
        let ghostty = parse_theme_text("background = 101010\nforeground = #eeeeee\nselection-background = #333333\npalette = 1=#ff0000\npalette = 3=#ffff00\npalette = 4=#0000ff\npalette = 8=#888888\nconfig-file = /tmp/other.conf", "ghost").unwrap();
        let values = ghostty[0]
            .1
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(values["base"], "#101010");
        assert_eq!(values["brand"], "#0000ff");
        assert_eq!(values["needs_you"], "#ffff00");
        assert!(!values.contains_key("config-file"));
        assert_eq!(values["inherit"], "footnote-superscript");
        assert_eq!(
            final_name("Catppuccin Mocha", &HashSet::new()).unwrap(),
            "catppuccin-mocha"
        );
        assert_eq!(
            final_name("midnight", &HashSet::from(["midnight".into()])).unwrap(),
            "midnight-2"
        );
        assert!(final_name("***", &HashSet::new()).is_err());
        let collision = candidate(
            "Catppuccin Mocha",
            vec![("base".into(), "#101010".into())],
            &HashSet::from(["catppuccin-mocha".into()]),
        )
        .unwrap();
        assert_eq!(collision.name, "catppuccin-mocha-2");
        assert!(collision
            .rename_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("already your theme")));
        let light = parse_theme_text("background = #ffffff", "light").unwrap();
        assert!(light[0]
            .1
            .iter()
            .any(|(key, value)| key == "inherit" && value == "footnote-paper"));
        let invalid_spec = parse_theme_text(
            "[mux.themes.bad]\nbase = \"#zzzzzz\"\nunknown = \"red\"\ninherit = \"not-a-built-in\"",
            "ignored",
        )
        .unwrap()
        .remove(0);
        let invalid = candidate(&invalid_spec.0, invalid_spec.1, &HashSet::new()).unwrap();
        assert_eq!(invalid.warnings.len(), 3);
        let refused_dir = tempfile::tempdir().unwrap();
        assert!(save_all(refused_dir.path(), &[invalid], "local", "2026-09-29").is_err());
        assert_eq!(fs::read_dir(refused_dir.path()).unwrap().count(), 0);
    }

    async fn assert_fetch_and_save_refuse_oversize_and_never_replace_a_theme() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("curl-stub.sh");
        let args_path = root.path().join("curl-args");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nhead -c 70000 /dev/zero\n",
                args_path.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();
        assert!(
            fetch_with(&program, "http://github.com/o/r/blob/main/theme.conf")
                .await
                .is_err()
        );
        assert!(!args_path.exists(), "a refused URL never reaches curl");
        assert!(
            fetch_with(&program, "https://github.com/o/r/blob/main/theme.conf")
                .await
                .unwrap_err()
                .contains("65,536")
        );
        let args = fs::read_to_string(&args_path).unwrap();
        assert!(args.starts_with("-q\n"));
        assert!(args.contains("--proto\n=https\n--proto-redir\n=https\n"));
        assert!(args.contains("--max-time\n10\n--max-filesize\n65536\n"));
        for forbidden in ["-H", "-u", "-b", "-K"] {
            assert!(!args.lines().any(|arg| arg == forbidden));
        }
        let valid_program = root.path().join("valid-curl.sh");
        let valid_args = root.path().join("valid-curl-args");
        fs::write(
            &valid_program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' 'background = 101010' 'foreground = #eeeeee' 'palette = 4=#0000ff'\n",
                valid_args.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&valid_program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&valid_program, permissions).unwrap();
        let bytes = fetch_with(
            &valid_program,
            "https://github.com/octo/themes/blob/main/mocha.conf",
        )
        .await
        .unwrap();
        let args = fs::read_to_string(valid_args).unwrap();
        assert!(args.ends_with("https://raw.githubusercontent.com/octo/themes/main/mocha.conf\n"));
        let ghostty = parse_theme_text(&String::from_utf8(bytes).unwrap(), "mocha").unwrap();
        assert_eq!(ghostty[0].0, "mocha");
        assert!(ghostty[0]
            .1
            .iter()
            .any(|(key, value)| key == "brand" && value == "#0000ff"));
        let item = Candidate {
            name: "midnight".into(),
            rename_reason: None,
            theme: Theme::default_theme(),
            spec: vec![("base".into(), "#112233".into())],
            warnings: Vec::new(),
        };
        let dir = root.path().join("themes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("midnight.toml"), "keep me").unwrap();
        assert_eq!(
            save_all(&dir, &[item], "local file", "2026-09-29").unwrap(),
            vec!["midnight-2"]
        );
        assert_eq!(
            fs::read_to_string(dir.join("midnight.toml")).unwrap(),
            "keep me"
        );
        assert!(fs::read_to_string(dir.join("midnight-2.toml"))
            .unwrap()
            .contains("base = \"#112233\""));

        let folder = root.path().join("batch");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("midnight.toml"), "base = \"#101010\"").unwrap();
        fs::write(folder.join("notes.txt"), "not a theme").unwrap();
        for index in 0..31 {
            fs::write(folder.join(format!("extra-{index}.txt")), "not a theme").unwrap();
        }
        assert!(preview(&Source::Folder(folder.clone()), root.path())
            .await
            .unwrap_err()
            .contains("32 files"));
        fs::remove_file(folder.join("extra-30.txt")).unwrap();
        let batch = preview(&Source::Folder(folder), root.path()).await.unwrap();
        assert_eq!(batch.candidates.len(), 1);
        assert_eq!(batch.skipped.len(), 31);
    }

    #[tokio::test]
    async fn import_sources_and_persistence_stay_bounded() {
        assert_local_and_github_sources_are_bounded_and_canonical().await;
        assert_fetch_and_save_refuse_oversize_and_never_replace_a_theme().await;
    }
}

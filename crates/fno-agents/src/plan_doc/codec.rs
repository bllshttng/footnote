//! Frontmatter codec for plan docs, ported 1:1 from `cli/src/fno/plan/_stamp.py`.
//!
//! Every scalar is kept as a string. Nested blocks stay opaque (`Value::Raw`
//! holds the child lines verbatim). A block list keeps block form
//! (`Value::BlockList`). Inline items are quoted by the unsafe-char rule plus
//! the inline-only needs-quote rule; block items by the unsafe-char rule only.
//! Key order is preserved. The write is atomic (tempfile in the same dir, then
//! rename) and keeps the target's file mode.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A parsed frontmatter value. Mirrors the Python `str | list | BlockList |
/// RawBlock` union; the variant IS the on-disk form.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Plain list, always emitted inline: `[a, b]`.
    List(Vec<String>),
    /// A list that arrived as a block sequence and is emitted as one.
    BlockList(Vec<String>),
    /// Opaque indented child lines, passed through verbatim.
    Raw(String),
    /// A scalar string (the parser reads every scalar raw).
    Scalar(String),
}

/// Insertion-ordered field map, the Python dict's order-preserving subset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fields {
    entries: Vec<(String, Value)>,
}

impl Fields {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Replace in place, or append a new key at the end (dict semantics).
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| k != key);
        self.entries.len() != before
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }
}

/// `parse_frontmatter`'s return: fields, raw block, rest of content.
#[derive(Debug)]
pub struct Parsed {
    pub fields: Fields,
    pub block: String,
    pub rest: String,
}

/// Read/parse failure. `NotFound` maps to exit 3 in set-expected, exit 1
/// elsewhere; `Parse` carries the Python `ValueError` message text.
#[derive(Debug)]
pub enum ReadError {
    NotFound(String),
    Parse(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotFound(m) | ReadError::Parse(m) => f.write_str(m),
        }
    }
}

// ---------------------------------------------------------------------------
// Quoting rules (the _YAML_UNSAFE_RE / _NEEDS_QUOTE_RE pair, hand-rolled)
// ---------------------------------------------------------------------------

/// An inline item carrying any of these cannot be written bare.
fn needs_quote(item: &str) -> bool {
    item.chars()
        .any(|c| matches!(c, ',' | '"' | '[' | ']' | '{' | '}'))
}

/// True when an unquoted item would not read back as itself. Shared by both
/// list forms; quoting is on structure, never on type.
fn bare_is_ambiguous(item: &str) -> bool {
    if item.is_empty() {
        return true;
    }
    let bytes = item.as_bytes();
    // ^[-?:](\s|$)
    if matches!(bytes[0], b'-' | b'?' | b':')
        && (bytes.len() == 1 || bytes[1].is_ascii_whitespace())
    {
        return true;
    }
    // ^[][{}#&*!|>%@`,'"]
    if matches!(
        bytes[0],
        b']' | b'['
            | b'{'
            | b'}'
            | b'#'
            | b'&'
            | b'*'
            | b'!'
            | b'|'
            | b'>'
            | b'%'
            | b'`'
            | b','
            | b'\''
            | b'"'
    ) {
        return true;
    }
    let chars: Vec<char> = item.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        // :(\s|$)
        if *c == ':' && (i + 1 == chars.len() || chars[i + 1].is_whitespace()) {
            return true;
        }
        // \s#
        if c.is_whitespace() && i + 1 < chars.len() && chars[i + 1] == '#' {
            return true;
        }
    }
    // ^\s | \s$
    chars.first().is_some_and(|c| c.is_whitespace())
        || chars.last().is_some_and(|c| c.is_whitespace())
}

/// Wrap an item in double quotes, escaping what the reader resolves.
fn quote_item(item: &str) -> String {
    let mut out = String::with_capacity(item.len() + 2);
    out.push('"');
    for c in item.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Emit an inline list item, quoting it when the form would be ambiguous.
fn serialize_item(item: &str) -> String {
    if needs_quote(item) || bare_is_ambiguous(item) {
        quote_item(item)
    } else {
        item.to_string()
    }
}

/// Emit a block sequence item, which carries a comma without quoting.
fn serialize_block_item(item: &str) -> String {
    if bare_is_ambiguous(item) {
        quote_item(item)
    } else {
        item.to_string()
    }
}

fn serialize_inline_list(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let formatted: Vec<String> = items.iter().map(|i| serialize_item(i)).collect();
    format!("[{}]", formatted.join(", "))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Strip surrounding quotes (single or double) from a scalar value. A
/// double-quoted value also has its `\"` / `\\` escapes resolved.
fn parse_scalar(raw: &str) -> String {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        return resolve_dq_escapes(&raw[1..raw.len() - 1]);
    }
    if bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        return raw[1..raw.len() - 1].to_string();
    }
    raw.to_string()
}

/// Resolve the `\"` / `\\` escapes the writer emits; other backslash sequences
/// stay verbatim (this reader is not a full YAML unescaper).
fn resolve_dq_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '"' || next == '\\' {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Split an inline-list body on commas that sit outside a double-quoted item.
/// Double quotes only; an unterminated quote yields one long item rather than
/// raising (a malformed doc must degrade).
fn split_inline_items(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in body.chars() {
        if escaped {
            escaped = false;
        } else if in_quote && ch == '\\' {
            escaped = true;
        } else if in_quote {
            in_quote = ch != '"';
        } else if ch == '"' {
            in_quote = true;
        } else if ch == ',' {
            items.push(std::mem::take(&mut buf));
            continue;
        }
        buf.push(ch);
    }
    items.push(buf);
    items
}

/// Parse an inline YAML list like `[a, b, c]` into items.
fn parse_inline_list(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if !(raw.starts_with('[') && raw.ends_with(']') && raw.len() >= 2) {
        return if raw.is_empty() {
            Vec::new()
        } else {
            vec![parse_scalar(raw)]
        };
    }
    let body = raw[1..raw.len() - 1].trim();
    if body.is_empty() {
        return Vec::new();
    }
    split_inline_items(body)
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .map(|item| parse_scalar(&item))
        .collect()
}

/// Locate the frontmatter block per the Python `^---\n(.*?)\n---(?:\n|$)`
/// DOTALL lazy match: the FIRST `\n---` that is followed by end-of-string or a
/// newline ends the block. Returns (block, rest) or None when content does not
/// start with `---\n`.
fn split_front(content: &str) -> Option<(String, String)> {
    let rest_str = content.strip_prefix("---\n")?;
    let bytes = rest_str.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i] == b'\n' && bytes[i + 1] == b'-' && bytes[i + 2] == b'-' && bytes[i + 3] == b'-'
        {
            let block = &rest_str[..i];
            // (?:\n|$): a trailing newline joins rest; end-of-string leaves rest empty.
            let rest = if i + 4 == bytes.len() {
                ""
            } else if bytes.get(i + 4) == Some(&b'\n') {
                &rest_str[i + 5..]
            } else {
                i += 1;
                continue;
            };
            return Some((block.to_string(), rest.to_string()));
        }
        i += 1;
    }
    None
}

/// Parse YAML frontmatter from markdown content. Content with no frontmatter
/// is not an error: it returns empty fields and the content untouched. Errors
/// carry the Python message text (they surface on stderr).
pub fn parse_frontmatter(content: &str) -> Result<Parsed, String> {
    let Some((block, rest)) = split_front(content) else {
        return Ok(Parsed {
            fields: Fields::new(),
            block: String::new(),
            rest: content.to_string(),
        });
    };

    let lines: Vec<&str> = block.lines().collect();
    let mut fields = Fields::new();
    let mut i = 0usize;
    // Last top-level key seen, so the nested-line error can name the key whose
    // value ran on.
    let mut last_key: Option<String> = None;

    while i < lines.len() {
        let line = lines[i];
        let lineno = i + 1;
        i += 1;
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        // Comment lines (template examples) must not crash the parser, and a
        // commented-out key clears the attribution rather than carrying it.
        if stripped.starts_with('#') {
            last_key = None;
            continue;
        }
        // An indented line here is leftover from an unclosed parent = error.
        if line.starts_with(' ') || line.starts_with('\t') {
            let whose = last_key
                .as_ref()
                .map(|k| format!(" (continuation of '{k}')"))
                .unwrap_or_default();
            return Err(format!(
                "Malformed frontmatter at line {lineno}{whose}: frontmatter scalars \
                 must be single-line - a wrapped or indented continuation line is \
                 not supported. Put the whole value on the key's own line. \
                 Offending line: '{line}'"
            ));
        }
        if !line.contains(':') {
            return Err(format!(
                "Malformed frontmatter at line {lineno}: cannot parse '{line}'"
            ));
        }
        let (key_raw, raw_val) = match line.split_once(':') {
            Some((k, v)) => (k, v),
            None => unreachable!(),
        };
        let key = key_raw.trim().to_string();
        let raw_val = raw_val.trim();
        last_key = Some(key.clone());

        if raw_val.starts_with('[') {
            fields.insert(key, Value::List(parse_inline_list(raw_val)));
        } else if raw_val.is_empty() {
            // Bare key: block-list-of-scalars first; on the first child line
            // that breaks that shape, switch to RawBlock pass-through.
            let start_idx = i;
            let mut items: Vec<String> = Vec::new();
            let mut raw_lines: Vec<&str> = Vec::new();
            let mut saw_child = false;
            let mut is_raw = false;
            while i < lines.len() {
                let child = lines[i];
                let child_stripped = child.trim();
                if child_stripped.is_empty() {
                    if is_raw {
                        raw_lines.push(child);
                    }
                    i += 1;
                    continue;
                }
                if !(child.starts_with(' ') || child.starts_with('\t')) {
                    break; // de-indented = block ended; outer loop re-processes
                }
                if child_stripped.starts_with('#') {
                    if is_raw {
                        raw_lines.push(child);
                    }
                    i += 1;
                    continue;
                }
                if is_raw {
                    raw_lines.push(child);
                    i += 1;
                    continue;
                }
                if let Some(rest) = child_stripped.strip_prefix("- ") {
                    items.push(parse_scalar(rest.trim()));
                    saw_child = true;
                    i += 1;
                    continue;
                }
                // Indented but not `- `: continuation of a mapping item -> raw.
                is_raw = true;
                raw_lines = lines[start_idx..i].to_vec();
                raw_lines.push(child);
                items.clear();
                saw_child = false;
                i += 1;
            }

            if is_raw {
                while raw_lines.last().is_some_and(|l| l.trim().is_empty()) {
                    raw_lines.pop();
                }
                fields.insert(key, Value::Raw(raw_lines.join("\n")));
            } else if saw_child {
                fields.insert(key, Value::BlockList(items));
            } else {
                fields.insert(key, Value::Scalar(String::new()));
            }
        } else {
            fields.insert(key, Value::Scalar(parse_scalar(raw_val)));
        }
    }

    Ok(Parsed {
        fields,
        block,
        rest,
    })
}

/// Serialize fields back to a frontmatter block (without --- delimiters).
pub fn serialize_frontmatter(fields: &Fields) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (key, value) in fields.iter() {
        match value {
            Value::Raw(text) => {
                lines.push(format!("{key}:"));
                if !text.is_empty() {
                    lines.push(text.clone());
                }
            }
            Value::BlockList(items) if !items.is_empty() => {
                lines.push(format!("{key}:"));
                for item in items {
                    lines.push(format!("  - {}", serialize_block_item(item)));
                }
            }
            Value::BlockList(items) | Value::List(items) => {
                // An empty block list falls through to inline `[]`.
                lines.push(format!("{}: {}", key, serialize_inline_list(items)));
            }
            Value::Scalar(text) => {
                let rendered = if bare_is_ambiguous(text) {
                    quote_item(text)
                } else {
                    text.clone()
                };
                lines.push(format!("{key}: {rendered}"));
            }
        }
    }
    lines.join("\n")
}

/// Read and parse a plan file. Returns (resolved target, fields, rest).
/// Epic-decomposition group nodes carry plan_path of the form
/// `<doc>#group-<slug>`; when the literal path is absent and dropping the
/// trailing `#group-` fragment yields a real file, use that.
pub fn read_plan_file(plan_path: &Path) -> Result<(PathBuf, Fields, String), ReadError> {
    let mut plan_path = plan_path.to_path_buf();
    if !plan_path.exists() {
        if let Some(name) = plan_path.file_name().and_then(|n| n.to_str()) {
            if let Some(pos) = name.rfind("#group-") {
                let stripped_name = &name[..pos];
                if !stripped_name.is_empty() {
                    let stripped = plan_path.with_file_name(stripped_name);
                    if stripped.exists() {
                        plan_path = stripped;
                    }
                }
            }
        }
    }

    if !plan_path.is_file() {
        return Err(ReadError::NotFound(format!(
            "Plan path does not exist: {}",
            plan_path.display()
        )));
    }

    let content = fs::read_to_string(&plan_path).map_err(|e| {
        ReadError::NotFound(format!(
            "Plan path does not exist: {} ({e})",
            plan_path.display()
        ))
    })?;
    let parsed = parse_frontmatter(&content).map_err(ReadError::Parse)?;
    Ok((plan_path, parsed.fields, parsed.rest))
}

/// Atomic write via tmp + rename, preserving the target's file mode.
pub fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let original_mode = fs::metadata(target).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        // Mask to the permission bits: mode() carries the file-type bits too.
        m.permissions().mode() & 0o777
    });
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = target.with_file_name(format!(
        ".{}.{}.{}.tmp",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        nanos
    ));
    let write = || -> std::io::Result<()> {
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(content.as_bytes())?;
        }
        if let Some(mode) = original_mode {
            let _ = fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
        }
        fs::rename(&tmp, target)
    };
    match write() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Serialize + write the updated frontmatter back to the plan file.
pub fn write_plan_file(target: &Path, fields: &Fields, rest: &str) -> std::io::Result<()> {
    let fm_block = serialize_frontmatter(fields);
    let new_content = format!("---\n{fm_block}\n---\n{rest}");
    atomic_write(target, &new_content)
}

// ---------------------------------------------------------------------------
// Tests (ported from test_stamp_frontmatter.py, test_stamp_wrapped_scalar.py,
// test_stamp_characterization.py, and the codec cases of test_stamp_plan.py)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_fields(fields: &Fields) -> Fields {
        let text = serialize_frontmatter(fields);
        let doc = format!("---\n{text}\n---\nbody\n");
        parse_frontmatter(&doc).unwrap().fields
    }

    fn get_list<'a>(f: &'a Fields, key: &str) -> &'a [String] {
        match f.get(key) {
            Some(Value::List(items)) | Some(Value::BlockList(items)) => items,
            other => panic!("expected list for {key}, got {other:?}"),
        }
    }

    fn get_str<'a>(f: &'a Fields, key: &str) -> &'a str {
        match f.get(key) {
            Some(Value::Scalar(s)) => s,
            other => panic!("expected scalar for {key}, got {other:?}"),
        }
    }

    const COMMA_ITEM: &str = "docs/report 1,000 items.md";

    #[test]
    fn comma_item_survives_three_roundtrips() {
        let doc = format!("---\nsources:\n  - {COMMA_ITEM}\n  - docs/b.md\n---\nbody\n");
        let parsed = parse_frontmatter(&doc).unwrap();
        assert_eq!(
            get_list(&parsed.fields, "sources"),
            [COMMA_ITEM, "docs/b.md"]
        );
        let mut fields = parsed.fields;
        for _ in 0..3 {
            fields = roundtrip_fields(&fields);
            assert_eq!(get_list(&fields, "sources"), [COMMA_ITEM, "docs/b.md"]);
        }
    }

    #[test]
    fn unquoted_comma_item_is_quoted_on_write() {
        let mut fields = Fields::new();
        fields.insert(
            "sources",
            Value::List(vec![COMMA_ITEM.to_string(), "b.md".to_string()]),
        );
        let fields = roundtrip_fields(&fields);
        assert_eq!(get_list(&fields, "sources"), [COMMA_ITEM, "b.md"]);
    }

    #[test]
    fn quoted_comma_item_parses_to_authored_value() {
        let parsed = parse_frontmatter("---\nsources: [\"a, b\", c]\n---\nbody\n").unwrap();
        assert_eq!(get_list(&parsed.fields, "sources"), ["a, b", "c"]);
    }

    #[test]
    fn item_containing_a_quote_character_roundtrips() {
        let items = vec!["he said \"hi\"".to_string(), "plain.md".to_string()];
        let mut fields = Fields::new();
        fields.insert("sources", Value::List(items.clone()));
        assert_eq!(get_list(&roundtrip_fields(&fields), "sources"), items);
    }

    #[test]
    fn apostrophe_item_roundtrips() {
        let items = vec!["don't split me".to_string(), "b.md".to_string()];
        let mut fields = Fields::new();
        fields.insert("sources", Value::List(items.clone()));
        assert_eq!(get_list(&roundtrip_fields(&fields), "sources"), items);
    }

    #[test]
    fn item_wrapped_in_apostrophes_keeps_them() {
        let items = vec!["'quoted'".to_string(), "b.md".to_string()];
        let mut fields = Fields::new();
        fields.insert("sources", Value::List(items.clone()));
        assert_eq!(get_list(&roundtrip_fields(&fields), "sources"), items);
    }

    fn yaml_unsafe_items() -> Vec<String> {
        vec![
            "fno x --json | grep -oE 'scope: [a-z]+'".to_string(),
            "a #b".to_string(),
            "- nested".to_string(),
            "key:".to_string(),
            "*alias".to_string(),
            "[x]".to_string(),
        ]
    }

    #[test]
    fn yaml_indicator_items_roundtrip_both_forms() {
        let items = yaml_unsafe_items();
        for block_form in [false, true] {
            let mut fields = Fields::new();
            fields.insert(
                "probes",
                if block_form {
                    Value::BlockList(items.clone())
                } else {
                    Value::List(items.clone())
                },
            );
            let text = serialize_frontmatter(&fields);
            // Every item must still be individually present and re-readable.
            let re = parse_frontmatter(&format!("---\n{text}\n---\nbody\n")).unwrap();
            assert_eq!(get_list(&re.fields, "probes"), items);
        }
        let mut fields = Fields::new();
        fields.insert("probes", Value::List(items.clone()));
        let mut cur = fields;
        for _ in 0..3 {
            cur = roundtrip_fields(&cur);
            assert_eq!(get_list(&cur, "probes"), items);
        }
    }

    #[test]
    fn plain_items_stay_bare() {
        let mut fields = Fields::new();
        fields.insert(
            "probes",
            Value::List(vec![
                "plain item".to_string(),
                "1".to_string(),
                "https://x.y/z".to_string(),
                "don't".to_string(),
                "issue#12".to_string(),
            ]),
        );
        assert_eq!(
            serialize_frontmatter(&fields),
            "probes: [plain item, 1, https://x.y/z, don't, issue#12]"
        );
    }

    #[test]
    fn unsafe_scalar_is_quoted_on_write() {
        let mut fields = Fields::new();
        fields.insert("title", Value::Scalar("a: b".to_string()));
        let text = serialize_frontmatter(&fields);
        let re = parse_frontmatter(&format!("---\n{text}\n---\nbody\n")).unwrap();
        assert_eq!(get_str(&re.fields, "title"), "a: b");
        assert_eq!(get_str(&roundtrip_fields(&fields), "title"), "a: b");
    }

    #[test]
    fn plain_scalar_stays_bare() {
        let mut fields = Fields::new();
        fields.insert("status", Value::Scalar("ready".to_string()));
        fields.insert("done_at", Value::Scalar("2026-09-15T00:00:00Z".to_string()));
        assert_eq!(
            serialize_frontmatter(&fields),
            "status: ready\ndone_at: 2026-09-15T00:00:00Z"
        );
    }

    #[test]
    fn empty_and_single_item_lists_keep_their_output() {
        let mut fields = Fields::new();
        fields.insert("urls", Value::List(vec![]));
        fields.insert("session_ids", Value::List(vec!["sess-1".to_string()]));
        let text = serialize_frontmatter(&fields);
        assert!(text.contains("urls: []"));
        assert!(text.contains("session_ids: [sess-1]"));
    }

    #[test]
    fn scalar_with_comma_is_untouched() {
        let parsed = parse_frontmatter("---\ntitle: a, b\n---\nbody\n").unwrap();
        assert_eq!(get_str(&parsed.fields, "title"), "a, b");
        assert!(serialize_frontmatter(&parsed.fields).contains("title: a, b"));
    }

    #[test]
    fn kill_criteria_rawblock_passes_through_byte_identical() {
        let block = "---\nkill_criteria:\n  - name: iteration_ceiling\n    predicate: iteration > 15\n    reason: Too many iterations, planning likely wrong\n---\nbody\n";
        let parsed = parse_frontmatter(block).unwrap();
        assert!(matches!(
            parsed.fields.get("kill_criteria"),
            Some(Value::Raw(_))
        ));
        assert_eq!(
            serialize_frontmatter(&parsed.fields),
            block[4..block.len() - 10].trim_end_matches('\n')
        );
    }

    #[test]
    fn no_frontmatter_returns_empty() {
        let parsed = parse_frontmatter("# just a body\n").unwrap();
        assert_eq!(parsed.fields, Fields::new());
        assert_eq!(parsed.block, "");
        assert_eq!(parsed.rest, "# just a body\n");
    }

    #[test]
    fn block_list_stays_block() {
        let doc = "---\nsources:\n  - docs/a.md\n  - docs/b.md\n---\nbody\n";
        let parsed = parse_frontmatter(doc).unwrap();
        assert_eq!(
            serialize_frontmatter(&parsed.fields),
            "sources:\n  - docs/a.md\n  - docs/b.md"
        );
    }

    #[test]
    fn added_keys_stay_inline() {
        let mut fields = parse_frontmatter("---\ntitle: t\n---\nbody\n")
            .unwrap()
            .fields;
        fields.insert("session_ids", Value::List(vec!["sess-1".to_string()]));
        assert!(serialize_frontmatter(&fields).contains("session_ids: [sess-1]"));
    }

    #[test]
    fn empty_block_list_falls_back_to_inline() {
        let mut fields = parse_frontmatter("---\nsources:\n  - only.md\n---\nbody\n")
            .unwrap()
            .fields;
        fields.insert("sources", Value::BlockList(vec![]));
        assert!(serialize_frontmatter(&fields).contains("sources: []"));
    }

    #[test]
    fn unbalanced_quote_degrades_without_raising() {
        let parsed = parse_frontmatter("---\nsources: [\"a, b]\n---\nbody\n").unwrap();
        assert!(matches!(parsed.fields.get("sources"), Some(Value::List(_))));
        let _ = serialize_frontmatter(&parsed.fields);
    }

    #[test]
    fn wrapped_scalar_names_the_key_and_the_rule() {
        let wrapped = "---\nnode: x-node\nkill_criteria: kill any lint whose false-positive rate exceeds its catch\n  rate in the first month\nstatus: in_review\n---\n\n# body\n";
        let err = parse_frontmatter(wrapped).unwrap_err();
        assert!(
            err.contains("kill_criteria"),
            "names the runaway key: {err}"
        );
        assert!(
            err.contains("must be single-line"),
            "states the rule: {err}"
        );
    }

    #[test]
    fn single_line_scalar_of_the_same_value_parses() {
        let doc = "---\nnode: x-node\nkill_criteria: kill any lint whose false-positive rate exceeds its catch rate in the first month\nstatus: in_review\n---\n\n# body\n";
        let parsed = parse_frontmatter(doc).unwrap();
        assert!(get_str(&parsed.fields, "kill_criteria").ends_with("in the first month"));
        assert_eq!(get_str(&parsed.fields, "status"), "in_review");
    }

    #[test]
    fn comment_between_key_and_continuation_names_no_key() {
        let doc = "---\nstatus: in_review\n# Optional: depends_on:\n  - x-node\n---\n\n# body\n";
        let err = parse_frontmatter(doc).unwrap_err();
        assert!(err.contains("must be single-line"));
        assert!(!err.contains("status"), "blamed the wrong key: {err}");
        assert!(
            !err.contains("continuation of"),
            "claimed an attribution it lacks: {err}"
        );
    }

    #[test]
    fn block_mapping_of_mappings_projects_shape() {
        let fm = "---\ntitle: Cross-project plan\ncreated: 2026-04-27\nscope: cross-project\nprojects:\n  fno:\n    repo: ~/code/me/fno\n    order: 1\n  chingu:\n    repo: ~/code/me/chingu\n    order: 2\nexpected_url_count: 2\n---\n\n# Body\n";
        let parsed = parse_frontmatter(fm).unwrap();
        match parsed.fields.get("projects") {
            Some(Value::Raw(text)) => {
                assert!(text.contains("fno:"));
                assert!(text.contains("chingu:"));
                assert!(text.contains("repo: ~/code/me/fno"));
            }
            other => panic!("projects should be Raw, got {other:?}"),
        }
        let serialized = serialize_frontmatter(&parsed.fields);
        assert!(serialized.contains("fno:"));
        assert!(serialized.contains("chingu:"));
        assert!(serialized.contains("repo: ~/code/me/chingu"));
    }

    #[test]
    fn block_list_of_scalars_still_works() {
        let fm = "---\nshipped_at: 2026-04-29T19:00:00Z\nurls:\n  - https://example.com/pull/1\n  - https://example.com/pull/2\nsession_ids:\n  - session-aaa\nstatus: shipped\n---\n\n# Body\n";
        let parsed = parse_frontmatter(fm).unwrap();
        assert_eq!(
            get_list(&parsed.fields, "urls"),
            ["https://example.com/pull/1", "https://example.com/pull/2"]
        );
        assert_eq!(get_list(&parsed.fields, "session_ids"), ["session-aaa"]);
    }

    fn write_tmp(tag: &str, name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-plan-doc-codec-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn read_plan_file_strips_group_fragment() {
        let doc = write_tmp(
            "grp",
            "design.md",
            "---\nstatus: shipped\nurls: [https://x/1]\n---\n# body\n",
        );
        let fragment = PathBuf::from(format!("{}#group-backend", doc.display()));
        let (target, fields, _) = read_plan_file(&fragment).unwrap();
        assert_eq!(target, doc);
        assert_eq!(get_str(&fields, "status"), "shipped");
    }

    #[test]
    fn read_plan_file_literal_hash_filename_wins() {
        let weird = write_tmp(
            "hash",
            "weird#group-x.md",
            "---\nstatus: shipped\n---\n# body\n",
        );
        let (target, fields, _) = read_plan_file(&weird).unwrap();
        assert_eq!(target, weird);
        assert_eq!(get_str(&fields, "status"), "shipped");
    }

    #[test]
    fn read_plan_file_non_group_fragment_fails_fast() {
        let dir = std::env::temp_dir().join(format!("fno-plan-doc-typo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec"), "---\nstatus: shipped\n---\n# body\n").unwrap();
        let typo = dir.join("spec#draft.md");
        assert!(matches!(read_plan_file(&typo), Err(ReadError::NotFound(_))));
    }

    #[test]
    fn read_plan_file_strips_only_trailing_group_fragment() {
        let doc = write_tmp("trail", "a#b.md", "---\nstatus: shipped\n---\n# body\n");
        let fragment = PathBuf::from(format!("{}#group-backend", doc.display()));
        let (target, _, _) = read_plan_file(&fragment).unwrap();
        assert_eq!(target, doc);
    }

    #[test]
    fn read_plan_file_group_fragment_without_base_file_fails_fast() {
        let dir = std::env::temp_dir().join(format!("fno-plan-doc-miss-{}", std::process::id()));
        let missing = dir.join("missing.md#group-api");
        assert!(matches!(
            read_plan_file(&missing),
            Err(ReadError::NotFound(_))
        ));
    }

    #[test]
    fn atomic_write_keeps_mode_and_replaces_content() {
        use std::os::unix::fs::PermissionsExt;
        let doc = write_tmp("mode", "plan.md", "---\nstatus: draft\n---\n# body\n");
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut fields = Fields::new();
        fields.insert("status", Value::Scalar("done".to_string()));
        write_plan_file(&doc, &fields, "# body\n").unwrap();
        let text = std::fs::read_to_string(&doc).unwrap();
        assert!(text.starts_with("---\nstatus: done\n---\n# body\n"));
        let mode = std::fs::metadata(&doc).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }
}

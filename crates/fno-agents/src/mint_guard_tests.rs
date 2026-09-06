//! Rust leg of the registry mint guard: every non-test `RegistryEntry`
//! literal must be built on `RegistryEntry::new`, which names the canonical
//! session identity and the parent edge positionally.
//!
//! `RegistryEntry` derives `Default`, so `RegistryEntry { name,
//! ..Default::default() }` compiles and mints a row with a null
//! `harness_session_id` and a null parent edge. The type system cannot catch
//! that, so this scan does: it fails, naming the site, on any literal whose
//! struct-update base is `..Default::default()` outside test code. `state.rs`
//! is the allowed exception as the struct's and the mint constructor's home,
//! mirroring registry.py on the Python leg
//! (cli/tests/unit/test_registry_mint_guard.py).

use std::fs;
use std::path::Path;

fn is_test_path(rel: &str) -> bool {
    rel.contains("/tests/") || rel.ends_with("_tests.rs")
}

/// One pass over (relpath, contents) pairs. Returns "rel:line" offender
/// strings and the count of non-test literal sites matched, so the caller can
/// tell an honest zero from a scan that never saw production code.
fn scan(sources: &[(String, String)]) -> (Vec<String>, usize) {
    let mut offenders = Vec::new();
    let mut sites = 0;
    for (rel, text) in sources {
        let (file_offenders, file_sites) = scan_one(rel, text);
        offenders.extend(file_offenders);
        sites += file_sites;
    }
    (offenders, sites)
}

fn scan_one(rel: &str, text: &str) -> (Vec<String>, usize) {
    if is_test_path(rel) {
        return (Vec::new(), 0);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut offenders = Vec::new();
    let mut sites = 0usize;
    let mut line = 1usize;
    let mut i = 0usize;
    let mut brace_depth = 0usize;
    // Brace depths at which open #[cfg(test)] regions close. A position is
    // test-scoped while any region's depth is still open.
    let mut test_until: Vec<usize> = Vec::new();

    // Lexing primitives shared by every pass. They skip comments, strings,
    // raw strings and char literals so a brace inside one cannot desync the
    // depth tracking; a desync that leaves a test region open would hide
    // offenders, so the lexer leans fail-closed.
    fn skip_ws_comments(chars: &[char], i: &mut usize, line: &mut usize) {
        loop {
            while *i < chars.len() && chars[*i].is_whitespace() {
                if chars[*i] == '\n' {
                    *line += 1;
                }
                *i += 1;
            }
            if *i + 1 < chars.len() && chars[*i] == '/' && chars[*i + 1] == '/' {
                while *i < chars.len() && chars[*i] != '\n' {
                    *i += 1;
                }
                continue;
            }
            if *i + 1 < chars.len() && chars[*i] == '/' && chars[*i + 1] == '*' {
                *i += 2;
                while *i + 1 < chars.len() && !(chars[*i] == '*' && chars[*i + 1] == '/') {
                    if chars[*i] == '\n' {
                        *line += 1;
                    }
                    *i += 1;
                }
                *i = (*i + 2).min(chars.len());
                continue;
            }
            return;
        }
    }

    fn skip_string(chars: &[char], i: &mut usize, line: &mut usize) {
        // chars[*i] == '"'. Handles escapes; raw strings are handled by the caller.
        *i += 1;
        while *i < chars.len() {
            if chars[*i] == '\\' {
                *i += 2;
                continue;
            }
            if chars[*i] == '"' {
                *i += 1;
                return;
            }
            if chars[*i] == '\n' {
                *line += 1;
            }
            *i += 1;
        }
    }

    fn skip_raw_string(chars: &[char], i: &mut usize, hashes: usize, line: &mut usize) {
        // chars[*i] points at the opening '"'.
        *i += 1;
        while *i < chars.len() {
            if chars[*i] == '"' {
                let mut n = 0;
                while n < hashes && *i + 1 + n < chars.len() && chars[*i + 1 + n] == '#' {
                    n += 1;
                }
                if n == hashes {
                    *i += 1 + hashes;
                    return;
                }
            }
            if chars[*i] == '\n' {
                *line += 1;
            }
            *i += 1;
        }
    }

    fn skip_char_or_lifetime(chars: &[char], i: &mut usize) {
        // chars[*i] == '\''. A char literal closes within two more chars
        // ('\'' or '\n'); anything else is a lifetime label.
        if *i + 1 < chars.len() && chars[*i + 1] == '\\' {
            *i += 2;
            while *i < chars.len() && chars[*i] != '\'' {
                *i += 1;
            }
            *i += 1;
            return;
        }
        if *i + 2 < chars.len() && chars[*i + 2] == '\'' {
            *i += 3;
            return;
        }
        *i += 1;
        while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_') {
            *i += 1;
        }
    }

    fn read_ident(chars: &[char], i: &mut usize) -> String {
        let start = *i;
        while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_') {
            *i += 1;
        }
        chars[start..*i].iter().collect()
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            '/' if i + 1 < chars.len() && (chars[i + 1] == '/' || chars[i + 1] == '*') => {
                skip_ws_comments(&chars, &mut i, &mut line);
            }
            '"' => skip_string(&chars, &mut i, &mut line),
            'r' if i + 1 < chars.len() && (chars[i + 1] == '#' || chars[i + 1] == '"') => {
                i += 1;
                let mut hashes = 0;
                while i < chars.len() && chars[i] == '#' {
                    hashes += 1;
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    skip_raw_string(&chars, &mut i, hashes, &mut line);
                }
            }
            'b' if i + 1 < chars.len() && (chars[i + 1] == '"' || chars[i + 1] == '\'') => {
                i += 1;
            }
            'b' if i + 2 < chars.len()
                && chars[i + 1] == 'r'
                && (chars[i + 2] == '#' || chars[i + 2] == '"') =>
            {
                i += 2;
                let mut hashes = 0;
                while i < chars.len() && chars[i] == '#' {
                    hashes += 1;
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    skip_raw_string(&chars, &mut i, hashes, &mut line);
                }
            }
            '\'' => skip_char_or_lifetime(&chars, &mut i),
            '{' => {
                brace_depth += 1;
                i += 1;
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                while test_until.last().map_or(false, |&d| brace_depth <= d) {
                    test_until.pop();
                }
                i += 1;
            }
            '#' => {
                let save = i;
                if !match_cfg_test(&chars, &mut i) {
                    i = save + 1;
                    continue;
                }
                // The attributed item: skip further attributes, then find the
                // item kind. Brace items (mod/fn) open a test region;
                // semicolon items (use/const/...) and path-wired `mod x;`
                // end at the semicolon.
                loop {
                    skip_ws_comments(&chars, &mut i, &mut line);
                    if i < chars.len() && chars[i] == '#' {
                        i += 1;
                        skip_ws_comments(&chars, &mut i, &mut line);
                        if i < chars.len() && chars[i] == '[' {
                            let mut depth = 0usize;
                            while i < chars.len() {
                                let ch = chars[i];
                                if ch == '[' {
                                    depth += 1;
                                } else if ch == ']' {
                                    depth -= 1;
                                    if depth == 0 {
                                        i += 1;
                                        break;
                                    }
                                }
                                i += 1;
                            }
                            continue;
                        }
                        break;
                    }
                    break;
                }
                skip_ws_comments(&chars, &mut i, &mut line);
                let mut kind = String::new();
                loop {
                    let save = i;
                    let word = read_ident(&chars, &mut i);
                    if word.is_empty() {
                        i = save;
                        break;
                    }
                    if matches!(
                        word.as_str(),
                        "pub" | "async" | "unsafe" | "extern" | "default"
                    ) {
                        skip_ws_comments(&chars, &mut i, &mut line);
                        if i < chars.len() && chars[i] == '(' {
                            // pub(crate) and friends.
                            let mut depth = 0usize;
                            while i < chars.len() {
                                if chars[i] == '(' {
                                    depth += 1;
                                } else if chars[i] == ')' {
                                    depth -= 1;
                                    if depth == 0 {
                                        i += 1;
                                        break;
                                    }
                                }
                                i += 1;
                            }
                            skip_ws_comments(&chars, &mut i, &mut line);
                        }
                        continue;
                    }
                    kind = word;
                    break;
                }
                match kind.as_str() {
                    "use" | "const" | "static" | "type" => {
                        while i < chars.len() && chars[i] != ';' {
                            if chars[i] == '\n' {
                                line += 1;
                            }
                            i += 1;
                        }
                        i += 1;
                    }
                    "mod" | "fn" | "impl" | "trait" | "struct" | "enum" | "union" => {
                        // Find the item's first brace; `mod name;` ends at ';'.
                        while i < chars.len() {
                            let ch = chars[i];
                            if ch == '\n' {
                                line += 1;
                            } else if ch == ';' {
                                i += 1;
                                break;
                            } else if ch == '{' {
                                test_until.push(brace_depth);
                                brace_depth += 1;
                                i += 1;
                                break;
                            }
                            i += 1;
                        }
                    }
                    _ => {}
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let start_line = line;
                let ident = read_ident(&chars, &mut i);
                if ident != "RegistryEntry" {
                    continue;
                }
                // `struct RegistryEntry {` and `impl RegistryEntry {` are the
                // declaration and its impl block, not literals.
                let ident_start = i - ident.chars().count();
                let mut j = ident_start;
                while j > 0 && chars[j - 1].is_whitespace() {
                    j -= 1;
                }
                let word_end = j;
                let mut word_start = word_end;
                while word_start > 0
                    && (chars[word_start - 1].is_alphanumeric() || chars[word_start - 1] == '_')
                {
                    word_start -= 1;
                }
                let prev_word: String = chars[word_start..word_end].iter().collect();
                if prev_word == "struct" || prev_word == "impl" || prev_word == "enum" {
                    continue;
                }
                skip_ws_comments(&chars, &mut i, &mut line);
                if i >= chars.len() || chars[i] != '{' {
                    continue;
                }
                // Brace-track the literal span, comment/string aware.
                let mut depth = 0usize;
                let mut span = String::new();
                loop {
                    if i >= chars.len() {
                        break;
                    }
                    let ch = chars[i];
                    match ch {
                        '\n' => {
                            line += 1;
                            span.push(ch);
                            i += 1;
                        }
                        '/' if i + 1 < chars.len()
                            && (chars[i + 1] == '/' || chars[i + 1] == '*') =>
                        {
                            let mark = i;
                            skip_ws_comments(&chars, &mut i, &mut line);
                            span.extend(chars[mark..i].iter());
                        }
                        '"' => {
                            let mark = i;
                            skip_string(&chars, &mut i, &mut line);
                            span.extend(chars[mark..i].iter());
                        }
                        'r' if i + 1 < chars.len()
                            && (chars[i + 1] == '#' || chars[i + 1] == '"') =>
                        {
                            let mark = i;
                            i += 1;
                            let mut hashes = 0;
                            while i < chars.len() && chars[i] == '#' {
                                hashes += 1;
                                i += 1;
                            }
                            if i < chars.len() && chars[i] == '"' {
                                skip_raw_string(&chars, &mut i, hashes, &mut line);
                            }
                            span.extend(chars[mark..i].iter());
                        }
                        '\'' => {
                            let mark = i;
                            skip_char_or_lifetime(&chars, &mut i);
                            span.extend(chars[mark..i].iter());
                        }
                        '{' => {
                            depth += 1;
                            span.push(ch);
                            i += 1;
                        }
                        '}' => {
                            depth = depth.saturating_sub(1);
                            span.push(ch);
                            i += 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        ch => {
                            span.push(ch);
                            i += 1;
                        }
                    }
                }
                let test_scoped = test_until.last().map_or(false, |&d| brace_depth > d);
                if test_scoped {
                    continue;
                }
                sites += 1;
                if span.contains("..Default::default()") {
                    let snippet: String = text
                        .lines()
                        .nth(start_line - 1)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    offenders.push(format!("{rel}:{start_line}: {snippet}"));
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    (offenders, sites)
}

/// Consume `#[cfg(test)]` at chars[*i]; true when matched, with *i past `]`.
/// Never mutates the caller's line counter: on a failed probe the caller
/// rolls the index back, and the line must roll back with it.
fn match_cfg_test(chars: &[char], i: &mut usize) -> bool {
    let mut j = *i;
    let next = |j: &mut usize| -> Option<char> {
        loop {
            while *j < chars.len() && chars[*j].is_whitespace() {
                *j += 1;
            }
            if *j + 1 < chars.len() && chars[*j] == '/' && chars[*j + 1] == '/' {
                while *j < chars.len() && chars[*j] != '\n' {
                    *j += 1;
                }
                continue;
            }
            return chars.get(*j).copied();
        }
    };
    for expected in ['#', '[', 'c', 'f', 'g', '(', 't', 'e', 's', 't', ')', ']'] {
        match next(&mut j) {
            Some(c) if c == expected => {
                j += 1;
            }
            _ => return false,
        }
    }
    *i = j;
    true
}

fn collect(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, root, out);
        } else if path.extension().map_or(false, |e| e == "rs") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(text) = fs::read_to_string(&path) {
                out.push((rel, text));
            }
        }
    }
}

fn scan_real_tree() -> (Vec<String>, usize, usize) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    collect(&src, &src, &mut sources);
    let files = sources.len();
    let (offenders, sites) = scan(&sources);
    (offenders, sites, files)
}

#[test]
fn scanner_detects_a_default_based_literal() {
    let fixture = "\nfn f() {\n    let row = RegistryEntry {\n        name: \"x\".into(),\n        ..Default::default()\n    };\n}\n";
    let (offenders, sites) = scan(&[("lib.rs".into(), fixture.into())]);
    assert_eq!(sites, 1);
    assert_eq!(offenders, vec!["lib.rs:3: let row = RegistryEntry {"]);
}

#[test]
fn scanner_reports_the_literal_line_when_the_brace_moves_down() {
    let fixture = "fn f() {\n    let row = RegistryEntry\n        .clone()\n        .into_owned()\n        .map(|r| RegistryEntry\n        {\n            name: \"x\".into(),\n            ..Default::default()\n        })\n        .unwrap();\n}\n";
    let (offenders, sites) = scan(&[("lib.rs".into(), fixture.into())]);
    assert_eq!(sites, 1);
    assert_eq!(offenders, vec!["lib.rs:5: .map(|r| RegistryEntry"]);
}

#[test]
fn scanner_ignores_test_regions_new_bases_and_decls() {
    let fixture = "#[cfg(test)]\nmod t {\n    fn g() {\n        let row = RegistryEntry {\n            name: \"x\".into(),\n            ..Default::default()\n        };\n    }\n}\nfn h() -> RegistryEntry {\n    RegistryEntry {\n        name: \"y\".into(),\n        ..RegistryEntry::new(None, crate::state::Lineage::captured((None, None, None)))\n    }\n}\npub struct RegistryEntry {\n    name: String,\n}\nimpl RegistryEntry {\n    fn m() {}\n}\n";
    let (offenders, sites) = scan(&[("lib.rs".into(), fixture.into())]);
    assert_eq!(offenders, Vec::<String>::new());
    assert_eq!(
        sites, 1,
        "only the ::new-based literal is a production site"
    );
}

#[test]
fn scanner_ignores_whole_file_test_trees() {
    let fixture = "fn f() {\n    RegistryEntry { name: \"x\".into(), ..Default::default() };\n}\n";
    let (offenders, sites) = scan(&[
        ("daemon/tests/gc_receipts.rs".into(), fixture.into()),
        ("state_lookup_tests.rs".into(), fixture.into()),
    ]);
    assert_eq!((offenders, sites), (Vec::<String>::new(), 0));
}

#[test]
fn production_tree_has_no_default_based_mints() {
    let (offenders, sites, files) = scan_real_tree();
    assert!(
        files > 20,
        "scan saw only {files} files; the walk is broken"
    );
    assert!(
        sites >= 4,
        "scanner matched only {sites} production RegistryEntry literal sites; the symbol match is broken"
    );
    assert!(
        offenders.is_empty(),
        "RegistryEntry literals minting from ..Default::default() outside test code:\n{}",
        offenders.join("\n")
    );
}

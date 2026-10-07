//! Build script: embed the source git revision into the fno-agents bins so a
//! built binary can self-report which commit it came from.
//!
//! Rust-side `fno doctor` staleness now keys on a rev baked INTO the binary
//! instead of the external `~/.fno/installed-rust-rev` marker (which was
//! written only by `fno doctor update`, so a bare `cargo install` or dirty dev build
//! was misjudged). `FNO_AGENTS_CRATES_REV` is the crates/ subtree rev the
//! verdict compares against the source's crates/ subtree rev ;
//! `FNO_AGENTS_GIT_REV` is the full HEAD identity. Both surface
//! via `fno-agents version --json`, so the verdict needs no marker.
//!
//! Always emits all three env vars (falling back to "unknown"/"0") so `env!`
//! in the crate compiles even when git is unavailable -- e.g. a crates.io
//! tarball build, where there is no `.git` (the crate is `publish = true`).

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "codegen/check_supersession.rs"]
mod check_supersession_codegen;

fn main() {
    // Produce the cross-tree copies instead of checking them; the two
    // events-schema syncs are the exception: they CHECK their tracked copies so
    // a build never dirties a stale checkout (see check_generated_copy). These
    // run before the env-var work so a build that later fails still leaves the
    // copies fresh.
    sync_harness_capabilities();
    sync_event_store();
    sync_module_copy("live_store_fence");
    sync_module_copy("store_conn");
    sync_module_copy("otel_read");
    sync_merge_posture();
    sync_page_reload();
    sync_spawn_phase();
    sync_model_tiers();
    sync_registry_schema();
    sync_events_limits();
    sync_events_schema();
    sync_check_supersession();
    sync_provider_key();

    let rev = git_rev().unwrap_or_else(|| "unknown".to_string());
    let dirty = git_dirty();
    // The crates/ subtree rev (last commit touching crates/) is the rev `fno
    // doctor` keys its rust-staleness verdict on. It must be the
    // SAME quantity Python's update._rust_subtree_rev computes -- the last
    // commit touching crates/ -- so the binary's self-reported rev and the
    // source rev compare apples-to-apples (both subtree revs, not HEAD).
    let crates_rev = git_crates_rev().unwrap_or_else(|| "unknown".to_string());

    // Both vars are ALWAYS set so `env!("FNO_AGENTS_GIT_REV")` never fails to
    // compile, regardless of whether git was reachable at build time.
    println!("cargo:rustc-env=FNO_AGENTS_GIT_REV={rev}");
    println!("cargo:rustc-env=FNO_AGENTS_GIT_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=FNO_AGENTS_CRATES_REV={crates_rev}");
    // Baked for state::source_root_for_exe: a detached OUT_DIR (a build-dir
    // override like the machine pool) marks this binary as a dev build whose
    // manifest dir can recover the source root.
    println!(
        "cargo:rustc-env=FNO_AGENTS_BUILD_OUT_DIR={}",
        std::env::var("OUT_DIR").unwrap_or_default()
    );

    // Rebuild when HEAD moves so an incremental dev build does not bake a stale
    // rev. (The install path -- `cargo install` -- always does a clean build, so
    // it is correct regardless; this is dev-iteration hygiene.) Best-effort:
    // a missing ref path just makes cargo re-run this script, never an error.
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(gitdir) = run("git", &["rev-parse", "--absolute-git-dir"]) {
        let gitdir = gitdir.trim();
        println!("cargo:rerun-if-changed={gitdir}/HEAD");
        if let Ok(head) = std::fs::read_to_string(format!("{gitdir}/HEAD")) {
            if let Some(reference) = head.strip_prefix("ref: ") {
                if let Some(path) = run("git", &["rev-parse", "--git-path", reference.trim()]) {
                    println!("cargo:rerun-if-changed={}", path.trim());
                }
            }
        }
    }
}

/// Full HEAD SHA, or `None` when git is unavailable / this is not a checkout.
fn git_rev() -> Option<String> {
    let out = run("git", &["rev-parse", "HEAD"])?;
    let rev = out.trim().to_string();
    if rev.is_empty() {
        None
    } else {
        Some(rev)
    }
}

/// True when `crates/` has uncommitted changes. Scoped to the same pathspec as
/// [`git_crates_rev`]: consumers pair `dirty` with `crates_rev` to decide whether
/// a binary matches its source, and a dirty file elsewhere in the repo says
/// nothing about that. Conservative: any git failure reports `false` (a
/// published/CI build is treated as clean rather than spuriously flagged dirty).
fn git_dirty() -> bool {
    let Some(top) = run("git", &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match run(
        "git",
        &["-C", top.trim(), "status", "--porcelain", "--", "crates/"],
    ) {
        Some(s) => !s.trim().is_empty(),
        None => false,
    }
}

/// Last commit SHA that touched `crates/`, or `None` when git is unavailable.
///
/// Mirrors Python `update._rust_subtree_rev` exactly: `git -C <repo-root> log -1
/// --format=%H -- crates/`. Resolving the repo root via `--show-toplevel` keeps
/// the pathspec correct regardless of build.rs's cwd (the crate dir).
fn git_crates_rev() -> Option<String> {
    let top = run("git", &["rev-parse", "--show-toplevel"])?;
    let top = top.trim();
    let out = run(
        "git",
        &["-C", top, "log", "-1", "--format=%H", "--", "crates/"],
    )?;
    let rev = out.trim().to_string();
    if rev.is_empty() {
        None
    } else {
        Some(rev)
    }
}

/// Repo root as a path, or `None` when git is unavailable (crates.io tarball).
fn repo_root() -> Option<PathBuf> {
    let top = run("git", &["rev-parse", "--show-toplevel"])?;
    let top = top.trim();
    if top.is_empty() {
        None
    } else {
        Some(PathBuf::from(top))
    }
}

/// Write `bytes` to `path` only when they differ from what is already there.
///
/// An unconditional write restamps the mtime on every build, which makes cargo
/// re-run downstream work forever. Write-on-difference converges. The write
/// lands in a sibling temp file and renames, so a concurrent reader (a
/// parallel `cargo build --workspace` compiling the copy) never sees a torn
/// file.
fn write_if_different(path: &Path, bytes: &[u8]) {
    if let Ok(existing) = std::fs::read(path) {
        if existing == bytes {
            return;
        }
    }
    let temp = path.with_extension(format!(
        "{}.tmp{}",
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default(),
        std::process::id()
    ));
    let written = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        println!(
            "cargo:warning=fno-agents build: could not write {}: {err}",
            path.display()
        );
    }
}

/// CHECK a tracked generated copy instead of writing it.
///
/// These copies ship in the crates.io packages (the generators' inputs cannot:
/// package include paths cannot leave the crate dir), so a build that rewrote
/// them dirtied the tree at any head whose committed copy lagged its source and
/// blocked the next git pull. A drift fails the build; `FNO_SYNC_EVENTS_SCHEMA=1`
/// restores produce behavior for the regen-and-commit flow.
fn check_generated_copy(path: &Path, bytes: &[u8], produce: bool) {
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        return;
    }
    if !produce {
        panic!(
            "tracked copy drifted from its source: {}. Rebuild once with \
             FNO_SYNC_EVENTS_SCHEMA=1 to regenerate it, then commit.",
            path.display()
        );
    }
    write_if_different(path, bytes);
}

/// PRODUCE the downstream copies of the capability table instead of checking
/// them.
///
/// `harness_capabilities.rs` `include_str!`s the canonical TOML, so the Rust
/// tree owns it. Two byte copies hang off it, both generated per build:
/// `cli/src/fno/agents/harness_capabilities.toml` (loaded as Python package
/// data) and `crates/fno/src/harness_capabilities.toml` (`include_str!`ed by
/// the mux's registry reader). The fno crate stays copy-fed rather than
/// dep-fed on purpose: the two crates publish to crates.io independently
/// (the crates-publish workflow states the order is irrelevant because fno
/// does not depend on fno-agents as a cargo dep), so a dep would tie fno's
/// publishability to a registry state that lags this repo. The developer who
/// edits the canonical is the developer who builds this crate, so the sync
/// happens where the edit happens and silent drift is impossible. The
/// rust-ci generated-copies dirty-tree step is the tripwire for a copy
/// edited by hand.
///
/// No-op when `cli/` or the sibling crate is absent: that is the `cargo
/// package` / crates.io tarball case, where the crate must still build
/// (`publish = true`).
fn sync_harness_capabilities() {
    println!("cargo:rerun-if-changed=src/harness_capabilities.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/harness_capabilities.toml");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/harness_capabilities.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
    let mux_copy = root.join("crates/fno/src/harness_capabilities.toml");
    if !mux_copy.is_file() {
        return;
    }
    write_if_different(&mux_copy, &bytes);
}

/// PRODUCE the fno crate's event-store copies from this crate's implementation.
///
/// The two crates publish independently, so fno must compile a checked-in copy
/// instead of depending on fno-agents. The module file feeds over minus its
/// test-module declaration; the observation submodule feeds verbatim. The
/// generated-copies CI step checks both copies against this owner after every
/// build.
fn sync_event_store() {
    println!("cargo:rerun-if-changed=src/event_store.rs");
    println!("cargo:rerun-if-changed=src/event_store/observation.rs");
    println!("cargo:rerun-if-changed=src/event_store/validate.rs");
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = repo_root() else { return };
    let canonical = manifest.join("src/event_store.rs");
    let Ok(text) = std::fs::read_to_string(&canonical) else {
        return;
    };
    // A Windows checkout can carry CRLF while the generated copies are LF:
    // the suffix strip below and write_if_different both compare LF shapes.
    let text = text.replace("\r\n", "\n");
    let copy = root.join("crates/fno/src/event_store.rs");
    if !copy.is_file() {
        return;
    }
    let body = text.strip_suffix("\n#[cfg(test)]\nmod tests;\n").expect(
        "src/event_store.rs must end with its test module declaration; update sync_event_store",
    );
    let body = body.trim_end_matches('\n');
    let out = format!(
        "// @generated by crates/fno-agents/build.rs from crates/fno-agents/src/event_store.rs. Do not edit.\n{body}\n"
    );
    write_if_different(&copy, out.as_bytes());
    let observation = manifest.join("src/event_store/observation.rs");
    let Ok(observation_text) = std::fs::read_to_string(&observation) else {
        return;
    };
    let observation_text = observation_text.replace("\r\n", "\n");
    let observation_copy = root.join("crates/fno/src/event_store/observation.rs");
    if !observation_copy.is_file() {
        return;
    }
    let out = format!(
        "// @generated by crates/fno-agents/build.rs from crates/fno-agents/src/event_store/observation.rs. Do not edit.\n{observation_text}"
    );
    write_if_different(&observation_copy, out.as_bytes());
    let validate_rs = manifest.join("src/event_store/validate.rs");
    let Ok(validate_text) = std::fs::read_to_string(&validate_rs) else {
        return;
    };
    let validate_text = validate_text.replace("\r\n", "\n");
    let validate_copy = root.join("crates/fno/src/event_store/validate.rs");
    if !validate_copy.is_file() {
        return;
    }
    let out = format!(
        "// @generated by crates/fno-agents/build.rs from crates/fno-agents/src/event_store/validate.rs. Do not edit.\n{validate_text}"
    );
    write_if_different(&validate_copy, out.as_bytes());
}

/// PRODUCE the fno crate's copy of a store module from this crate's
/// implementation.
///
/// Same publish story as [`sync_event_store`]: the live-store fence and the
/// store-connection seam guard and open the operator stores in both
/// binaries, and fno compiles a generated copy. The owner's inline test
/// module stays with the owner; the copy is the code alone.
fn sync_module_copy(name: &str) {
    println!("cargo:rerun-if-changed=src/{name}.rs");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("src/{name}.rs"));
    let Ok(text) = std::fs::read_to_string(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let copy = root.join(format!("crates/fno/src/{name}.rs"));
    if !copy.is_file() && name != "otel_read" {
        return;
    }
    let marker = "\n#[cfg(test)]\nmod tests {";
    assert_eq!(
        text.matches(marker).count(),
        1,
        "src/{name}.rs must carry exactly one test module; update sync_module_copy"
    );
    let body = text
        .split_once(marker)
        .expect("matches() above guarantees the marker")
        .0
        .trim_end_matches('\n');
    let body = if name == "otel_read" {
        println!("cargo:rerun-if-changed=src/otel_schema.sql");
        let schema = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/otel_schema.sql"),
        )
        .expect("canonical OTEL schema must be readable");
        body.replace("include_str!(\"otel_schema.sql\")", &format!("{schema:?}"))
    } else {
        body.to_string()
    };
    let out = format!(
        "// @generated by crates/fno-agents/build.rs from crates/fno-agents/src/{name}.rs. Do not edit.\n{body}\n"
    );
    write_if_different(&copy, out.as_bytes());
}

/// PRODUCE the downstream copy of the merge-posture carrier table.
///
/// Same shape as [`sync_harness_capabilities`]: the canonical TOML lives in
/// this crate (`merge_posture.rs` `include_str!`s it), and the Python package
/// reads a byte copy as package data so a binary-less interpreter still
/// resolves the carrier vocabulary. The only copy is the Python tree's, so
/// this is one write, not two.
fn sync_merge_posture() {
    println!("cargo:rerun-if-changed=src/merge_posture.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/merge_posture.toml");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/merge_posture.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
}

/// PRODUCE the downstream copy of the operator-page reload script.
///
/// The crate owns `src/page_reload.js` (`king_ledger.rs` `include_str!`s it),
/// and the board renderer reads the byte copy
/// `cli/src/fno/graph/page_reload.js` as package data. The cli-ci
/// generated-copies step is the tripwire for a hand edit to the copy.
fn sync_page_reload() {
    println!("cargo:rerun-if-changed=src/page_reload.js");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/page_reload.js");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/graph/page_reload.js");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
}

/// PRODUCE the downstream copy of the spawn verb-to-phase table.
///
/// Same shape as [`sync_merge_posture`]: the canonical TOML lives in this
/// crate, and the Python package reads a byte copy as package data so
/// `infer_phase` and any future Rust reader cannot drift.
fn sync_spawn_phase() {
    println!("cargo:rerun-if-changed=src/spawn_phase.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/spawn_phase.toml");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/spawn_phase.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
}

/// PRODUCE the downstream copy of the inline slot-lane vocabulary.
///
/// Same shape as [`sync_spawn_phase`]: the canonical TOML lives in this
/// crate (`route_slot.rs` `include_str!`s it), and the Python package reads
/// a byte copy as package data so the lanes JSON projection cannot drift
/// from the Rust fold's field vocabulary.
/// PRODUCE the downstream copy of the reachability + static tier tables.
///
/// The canonical TOML lives in this crate
/// (`route_gather.rs` `include_str!`s it), and `benchmarks.py` reads the byte
/// copy as package data, so the Python tier read cannot drift from the Rust
/// inventory fold's built-in rows.
fn sync_model_tiers() {
    println!("cargo:rerun-if-changed=src/model_tiers.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/model_tiers.toml");
    let Ok(bytes) = std::fs::read(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/model_tiers.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, &bytes);
}

/// Render the registry schema version and writer floor from their single owner,
/// then project the Python copy.
///
/// `src/registry_schema.toml` owns both values. `state.rs` `include!`s
/// the generated constants, and registry.py reads the projected byte copy
/// `cli/src/fno/agents/registry_schema.toml` as package data, so a bump is
/// one edit in one file. This replaces the parity script that compared two
/// independent literals; the rust-ci generated-copies dirty-tree step is the
/// tripwire for a hand edit to the projected copy.
fn sync_registry_schema() {
    println!("cargo:rerun-if-changed=src/registry_schema.toml");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/registry_schema.toml");
    let text = std::fs::read_to_string(&canonical)
        .expect("src/registry_schema.toml must exist (it owns the schema and writer floor)");
    let parsed: toml::Value =
        toml::from_str(&text).expect("src/registry_schema.toml must parse as TOML");
    let version = parsed
        .get("version")
        .and_then(|value| value.as_integer())
        .expect("src/registry_schema.toml must carry an integer `version`");
    let min_writer = parsed
        .get("min_writer")
        .and_then(|value| value.as_integer())
        .expect("src/registry_schema.toml must carry an integer `min_writer`");
    assert!(
        min_writer <= version,
        "src/registry_schema.toml `min_writer` must not exceed `version`"
    );
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    std::fs::write(
        out_dir.join("registry_schema.rs"),
        format!(
            "pub const REGISTRY_SCHEMA_VERSION: u32 = {version};\npub const REGISTRY_MIN_WRITER_VERSION: u32 = {min_writer};\n"
        ),
    )
    .expect("generated registry_schema.rs must be writable");

    let Some(root) = repo_root() else { return };
    let cli_copy = root.join("cli/src/fno/agents/registry_schema.toml");
    if !cli_copy.is_file() {
        return;
    }
    write_if_different(&cli_copy, text.as_bytes());
}

/// CHECK `src/events_limits.toml` against the Python-owned event schema.
///
/// `cli/src/fno/events/schema.yaml` is canonical and Python reads its `limits`
/// block at runtime. Rust used to MIRROR the two scalars as literals in
/// `verify_evidence.rs`, linked only by a comment; you cannot generate from a
/// comment. Now the build renders the block into a tracked TOML sibling that
/// `events_limits.rs` `include_str!`s, so the committed file is what a
/// crates.io build compiles against and the link is a real dependency edge.
///
/// No-op when the schema is absent (tarball case) and on any parse failure: the
/// committed file is then the value, and the build-time drift check is the
/// tripwire against a hand edit.
fn sync_events_limits() {
    let generated = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/events_limits.toml");
    let Some(root) = repo_root() else { return };
    let schema = root.join("cli/src/fno/events/schema.yaml");
    if !schema.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", schema.display());
    let Ok(text) = std::fs::read_to_string(&schema) else {
        return;
    };
    let parsed: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            println!("cargo:warning=fno-agents build: schema.yaml did not parse: {err}");
            return;
        }
    };
    let limits = &parsed["limits"];
    let (Some(max_data_bytes), Some(encoding)) = (
        limits["max_data_bytes"].as_u64(),
        limits["data_size_encoding"].as_str(),
    ) else {
        println!("cargo:warning=fno-agents build: schema.yaml limits block incomplete");
        return;
    };
    let produce = std::env::var_os("FNO_SYNC_EVENTS_SCHEMA").is_some_and(|v| v == "1");
    check_generated_copy(
        &generated,
        render_events_limits(max_data_bytes, encoding).as_bytes(),
        produce,
    );
}

/// CHECK the native judge's rule set against the Python-owned event schema.
///
/// `cli/src/fno/events/schema.yaml` is canonical; `src/event_store/validate.rs`
/// `include_str!`s its JSON projection (`events_schema.json`) so the judge
/// needs no YAML parser and cannot drift from the schema Python reads. Every
/// `data.properties.<field>.enum` projects as `<field>`, and a list item's
/// enum as `<field>[].<sub>` (the review_attestation dispositions rule). A
/// sibling copy lives in crates/fno beside its generated `validate.rs`.
/// No-op when the schema is absent (tarball case); parse failures warn and
/// skip; a tracked copy that drifted fails the build (see
/// [`check_generated_copy`]).
fn sync_events_schema() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = repo_root() else { return };
    let schema = root.join("cli/src/fno/events/schema.yaml");
    if !schema.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", schema.display());
    let Ok(text) = std::fs::read_to_string(&schema) else {
        return;
    };
    let parsed: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            println!("cargo:warning=fno-agents build: schema.yaml did not parse: {err}");
            return;
        }
    };
    let mut event_types = serde_json::Map::new();
    for entry in parsed["event_types"].as_sequence().into_iter().flatten() {
        let Some(name) = entry["name"].as_str() else {
            continue;
        };
        let data = &entry["data"];
        let mut enums = serde_json::Map::new();
        for (field, prop) in data["properties"].as_mapping().into_iter().flatten() {
            let Some(field_name) = field.as_str() else {
                continue;
            };
            if prop["enum"].as_sequence().is_some() {
                enums.insert(field_name.to_string(), seq_to_json(&prop["enum"]));
            }
            if let Some(subs) = prop["items"]["properties"].as_mapping() {
                for (sub, subprop) in subs {
                    let Some(sub_name) = sub.as_str() else {
                        continue;
                    };
                    if subprop["enum"].as_sequence().is_some() {
                        enums.insert(
                            format!("{field_name}[].{sub_name}"),
                            seq_to_json(&subprop["enum"]),
                        );
                    }
                }
            }
        }
        event_types.insert(
            name.to_string(),
            serde_json::json!({
                "sources": seq_to_json(&entry["sources"]),
                "required": seq_to_json(&data["required"]),
                "forbidden": seq_to_json(&data["forbidden"]),
                "enums": enums,
            }),
        );
    }
    let family = &parsed["protocol_family"];
    let projection = serde_json::json!({
        "envelope_required": seq_to_json(&parsed["envelope"]["required"]),
        "events_schema_marker": "GENERATED by crates/fno-agents/build.rs from cli/src/fno/events/schema.yaml. Do not edit; rebuild instead.",
        "allowed_sources": seq_to_json(&parsed["envelope"]["properties"]["source"]["enum"]),
        "source_patterns": seq_to_json(&parsed["envelope"]["properties"]["source"]["patterns"]),
        "max_data_bytes": parsed["limits"]["max_data_bytes"].as_u64().unwrap_or(65536),
        "gates": seq_to_json(&parsed["gates"]),
        "protocol_family": {
            "types": seq_to_json(&family["types"]),
            "version": family["version"].as_i64().unwrap_or(1),
            "envelope_allowed": seq_to_json(&family["envelope"]["allowed"]),
            "envelope_required": seq_to_json(&family["envelope"]["required"]),
            "outcome_enum": seq_to_json(&family["outcome"]["enum"]),
            "outcome_present_on": seq_to_json(&family["outcome"]["present_on"]),
        },
        "event_types": event_types,
    });
    let body = serde_json::to_vec(&projection).expect("schema projection serializes");
    let produce = std::env::var_os("FNO_SYNC_EVENTS_SCHEMA").is_some_and(|v| v == "1");
    check_generated_copy(
        &manifest.join("src/event_store/events_schema.json"),
        &body,
        produce,
    );
    check_generated_copy(
        &root.join("crates/fno/src/event_store/events_schema.json"),
        &body,
        produce,
    );
}

/// YAML sequence to a JSON array of scalars. The schema blocks the judge
/// reads are lists of strings and numbers; anything else degrades to null
/// rather than failing the build.
fn seq_to_json(v: &serde_yaml_ng::Value) -> serde_json::Value {
    serde_json::Value::Array(
        v.as_sequence()
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item {
                        serde_yaml_ng::Value::String(s) => serde_json::Value::String(s.clone()),
                        serde_yaml_ng::Value::Number(n) => {
                            // Keep ints exact so enum reprs match Python's
                            // (1, never 1.0); only true floats go via f64.
                            if let Some(i) = n.as_i64() {
                                serde_json::Value::Number(i.into())
                            } else if let Some(u) = n.as_u64() {
                                serde_json::Value::Number(u.into())
                            } else {
                                serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0))
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                        }
                        serde_yaml_ng::Value::Bool(b) => serde_json::Value::Bool(*b),
                        _ => serde_json::Value::Null,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Generate the shared latest-attempt selector for both runtime languages.
fn sync_check_supersession() {
    let contract_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("codegen/check_supersession.toml");
    println!("cargo:rerun-if-changed={}", contract_path.display());
    println!("cargo:rerun-if-changed=codegen/check_supersession.rs");
    let contract = check_supersession_codegen::load_contract(&contract_path)
        .expect("check_supersession.toml must parse");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    std::fs::write(
        out_dir.join("check_supersession.rs"),
        check_supersession_codegen::render_rust(&contract),
    )
    .expect("generated Rust supersession source must be writable");

    let Some(root) = repo_root() else { return };
    let cli = root.join("cli");
    if cli.is_dir() {
        write_if_different(
            &cli.join("src/fno/pr/_check_supersession_generated.py"),
            check_supersession_codegen::render_python(&contract).as_bytes(),
        );
    }
}

/// PRODUCE the fno crate's provider-key copy from this crate's implementation.
///
/// Same publish story as [`sync_module_copy`]: the key rule is one
/// canonical file whose copy the composer crate compiles (crates/fno does not
/// depend on fno-agents), so the two cannot drift. The owner's inline test
/// module stays with the owner; the copy is the code alone.
fn sync_provider_key() {
    println!("cargo:rerun-if-changed=src/provider_key.rs");
    let canonical = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/provider_key.rs");
    let Ok(text) = std::fs::read_to_string(&canonical) else {
        return;
    };
    let Some(root) = repo_root() else { return };
    let copy = root.join("crates/fno/src/provider_key.rs");
    if !copy.is_file() {
        return;
    }
    let marker = "\n#[cfg(test)]\nmod tests {";
    assert_eq!(
        text.matches(marker).count(),
        1,
        "src/provider_key.rs must carry exactly one test module; update sync_provider_key"
    );
    let body = text
        .split_once(marker)
        .expect("matches() above guarantees the marker")
        .0
        .trim_end_matches('\n');
    let out = format!(
        "// @generated by crates/fno-agents/build.rs from crates/fno-agents/src/provider_key.rs. Do not edit.\n{body}\n"
    );
    write_if_different(&copy, out.as_bytes());
}

/// Render the generated `events_limits.toml` body. The CI tripwire renders the
/// same shape from the same source, so the two can be read against each other.
fn render_events_limits(max_data_bytes: u64, encoding: &str) -> String {
    format!(
        "# GENERATED by crates/fno-agents/build.rs from cli/src/fno/events/schema.yaml.\n\
         # Do not edit. Change the limits block in schema.yaml and rebuild.\n\
         max_data_bytes = {max_data_bytes}\n\
         data_size_encoding = \"{encoding}\"\n"
    )
}

/// Run a command, returning trimmed stdout on a zero exit, else `None`.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

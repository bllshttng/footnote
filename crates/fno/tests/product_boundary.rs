//! The product boundary, proven against the real manifests and the real
//! resolvers: the compile edge stays dev-only, the mux never links the
//! runtime, and a requested graph operation without its worker produces the
//! classified refusal naming the component and the repair.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Every test here either spawns `cargo metadata` (which inherits this
/// process's env) or mutates worker-resolver env. Serializing all three
/// keeps the inherited env deterministic under any test runner.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves from the crate manifest")
}

/// Run `cargo metadata --no-deps --offline` for one manifest and return the
/// named package's dependencies as (name, kind) pairs. kind is None for a
/// normal dependency, Some("dev") for dev, Some("build") for build.
fn package_deps(manifest: &Path, package: &str) -> Vec<(String, Option<String>)> {
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata succeeded for {}: {}",
        manifest.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");
    let packages = doc["packages"].as_array().expect("packages array");
    assert!(
        !packages.is_empty(),
        "positive control: metadata saw packages for {}",
        manifest.display()
    );
    let pkg = packages
        .iter()
        .find(|p| p["name"] == package)
        .unwrap_or_else(|| panic!("package {package} appears in its own metadata"));
    let deps = pkg["dependencies"].as_array().expect("dependencies array");
    deps.iter()
        .map(|d| {
            (
                d["name"].as_str().unwrap_or_default().to_string(),
                d["kind"].as_str().map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn fno_agents_links_fno_only_as_a_dev_dependency() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let manifest = repo_root().join("crates/fno-agents/Cargo.toml");
    let deps = package_deps(&manifest, "fno-agents");
    let fno_links: Vec<Option<String>> = deps
        .iter()
        .filter(|(name, _)| name == "fno")
        .map(|(_, kind)| kind.clone())
        .collect();
    assert!(
        !fno_links.is_empty(),
        "positive control: the dev edge is present in the manifest"
    );
    for kind in &fno_links {
        assert_eq!(
            kind.as_deref(),
            Some("dev"),
            "fno-agents links fno only as a dev dependency"
        );
    }
}

#[test]
fn the_mux_never_links_the_runtime() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let manifest = repo_root().join("crates/fno/Cargo.toml");
    let deps = package_deps(&manifest, "fno");
    let runtime_links: Vec<&(String, Option<String>)> = deps
        .iter()
        .filter(|(name, _)| name == "fno-agents")
        .collect();
    assert!(
        runtime_links.is_empty(),
        "the mux crate carries no dependency on fno-agents at all: {runtime_links:?}"
    );
}

#[test]
fn a_requested_graph_operation_without_its_worker_names_component_and_repair() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let graph = std::env::temp_dir().join(format!("fno-pb-graph-{}.json", std::process::id()));
    let var = "FNO_AGENTS_WORKER";
    let saved = std::env::var_os(var);
    let saved_path = std::env::var_os("PATH");
    // A resolver miss through the documented override seam: the typed refusal
    // must name the component and the repair, never an anonymous IO error.
    // Sanitizing PATH too: an installed worker on the test machine's PATH
    // would satisfy the resolver and turn this refusal into a success.
    std::env::set_var(var, "/nonexistent/fno-agents-worker");
    std::env::set_var("PATH", "/nonexistent");
    let result = fno::store_client::call(&graph, "begin", serde_json::json!({}));
    match saved {
        Some(v) => std::env::set_var(var, v),
        None => std::env::remove_var(var),
    }
    match saved_path {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }
    let err = result
        .err()
        .expect("the store call refuses without a worker");
    assert!(
        err.contains("fno-agents-worker"),
        "names the component: {err}"
    );
    assert!(err.contains("FNO_AGENTS_WORKER"), "names the repair: {err}");
}

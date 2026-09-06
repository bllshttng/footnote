//! The events envelope size limits, generated from the Python-owned schema.
//!
//! `cli/src/fno/events/schema.yaml` is canonical: Python reads its `limits`
//! block at runtime (`events/__init__.py`). Rust used to MIRROR the two scalars
//! as literals linked only by a comment, which is how a bumped limit shipped
//! half-applied. `build.rs` now renders the block into `events_limits.toml`,
//! this module `include_str!`s that file, and the link is a dependency edge
//! instead of prose.
//!
//! The committed TOML is what a crates.io build compiles against, so the crate
//! still builds with no `cli/` tree. A hand edit of the copy is caught by the
//! rust-ci generated-copies dirty-tree step.

use std::sync::OnceLock;

use serde::Deserialize;

const LIMITS_TOML: &str = include_str!("events_limits.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsLimits {
    max_data_bytes: usize,
    data_size_encoding: String,
}

fn limits() -> &'static EventsLimits {
    static LIMITS: OnceLock<EventsLimits> = OnceLock::new();
    LIMITS.get_or_init(|| {
        toml::from_str(LIMITS_TOML).expect("generated events_limits.toml must parse")
    })
}

/// Size cap for one event's `data` payload, in bytes.
pub fn max_data_bytes() -> usize {
    limits().max_data_bytes
}

/// The encoding the size cap is measured in.
pub fn data_size_encoding() -> &'static str {
    &limits().data_size_encoding
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_limits_parse() {
        assert!(max_data_bytes() > 0);
        assert!(!data_size_encoding().is_empty());
    }
}

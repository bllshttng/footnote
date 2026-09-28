//! Print the native command inventory from the one typed clap tree.
//! Regenerate the committed artifact with:
//! `cargo run --quiet --manifest-path crates/fno/Cargo.toml --example native_command_tree > scripts/ci/native-command-tree.txt`

fn main() {
    print!("{}", fno::cli_args::render_inventory());
}

//! Shell completion generated from the one typed clap tree:
//! `fno mux shell-init <zsh|bash>` appends this after the OSC 133 snippet, so
//! the same eval that arms command-block markers registers completion for
//! every declared native path. Root words that still forward to the Python
//! CLI are NOT in the tree, so both wrappers fall back to the shell's default
//! completion for them - a native-only list on `fno backlog <TAB>` would be a
//! regression, not a partial ship.

use clap_complete::Shell;

/// The root words whose completion the native tree owns today. Anything else
/// at the first word falls back to the shell's default completion (units 2-4
/// of the command-tree migration move the forwarded roots in).
fn native_roots() -> Vec<String> {
    let mut roots: Vec<String> = crate::cli_args::front_command()
        .get_subcommands()
        .filter(|sub| sub.get_name() != "help")
        .map(|sub| sub.get_name().to_string())
        .collect();
    roots.sort();
    roots
}

/// The raw `clap_complete` script for one shell.
fn generate(shell: Shell) -> String {
    let mut buf = Vec::new();
    let mut cmd = crate::cli_args::front_command();
    clap_complete::generate(shell, &mut cmd, "fno", &mut buf);
    String::from_utf8(buf).expect("completion script is UTF-8")
}

/// The zsh script: the generated completion registered only when `compdef`
/// exists (a shell without compinit must not print an error), with the
/// generated `_fno` preserved byte-stable as `_fno_native` behind a wrapper
/// that falls back to `_default` for words outside the native tree.
pub fn zsh_script() -> String {
    let generated = generate(Shell::Zsh);
    let roots = native_roots().join(" ");
    format!(
        "if (( $+functions[compdef] )); then\n\
         {generated}\
         functions[_fno_native]=$functions[_fno]\n\
         local -a _fno_native_roots\n\
         _fno_native_roots=({roots})\n\
         _fno() {{\n\
         \x20 if (( CURRENT > 2 )) || (( ${{_fno_native_roots[(Ie)$words[2]]}} )); then\n\
         \x20   _fno_native \"$@\"\n\
         \x20 else\n\
         \x20   _default\n\
         \x20 fi\n\
         }}\n\
         fi\n"
    )
}

/// The bash script: the generated completion plus one overriding `complete`
/// with `-o default`, so an unknown root word (where `_fno` produces nothing)
/// falls back to bash's default completion instead of an empty list.
pub fn bash_script() -> String {
    let generated = generate(Shell::Bash);
    format!(
        "{generated}\
         # Unknown root words fall back to the shell's default completion (the\n\
         # Python surface is not in the native tree yet).\n\
         complete -o default -o bashdefault -F _fno fno\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_registers_only_when_compdef_exists_and_falls_back() {
        let text = zsh_script();
        assert!(text.contains("#compdef fno"), "generated zsh header kept");
        assert!(text.starts_with("if (( $+functions[compdef] )); then"));
        assert!(text.contains("_default"), "fallback for foreign roots");
        assert!(text.contains("_fno_native_roots=(mux version)"));
        // The fallback decision happens at the FIRST word only.
        assert!(text.contains("CURRENT > 2"));
    }

    #[test]
    fn zsh_offers_native_paths_but_not_forwarded_roots() {
        let text = zsh_script();
        assert!(text.contains("reap"), "web reap completes");
        assert!(text.contains("retire-session"));
        assert!(
            !text.contains("backlog"),
            "forwarded roots must not complete from the native tree"
        );
    }

    #[test]
    fn bash_falls_back_to_default_completion() {
        let text = bash_script();
        assert!(text.contains("complete -o default -o bashdefault -F _fno fno"));
        assert!(text.contains("compdef") == false);
        assert!(!text.contains("backlog"));
    }
}

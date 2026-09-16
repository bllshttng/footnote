//! The `mux block` argv grammars (`block pipe`, `block annotate`): pure
//! parsers, unit-testable without a socket. A child module of `mux_cli` on
//! purpose: the file-budget gate keeps the parent shrink-only, and
//! `use super::*` keeps the shared cli_args flag groups in one place.
use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParsedBlockPipe {
    pub(super) session: Option<String>,
    pub(super) json: bool,
    pub(super) from: u64,
    pub(super) to: u64,
    pub(super) block: BlockSel,
    pub(super) force: bool,
}

/// Parse the tokens after `mux block` into a [`ParsedBlockPipe`]. Pure, so the
/// grammar is unit-testable without a socket. `pipe` is the only block verb.
pub(super) fn parse_block_args(args: &[OsString]) -> Result<ParsedBlockPipe, String> {
    // The typed tree already routed on the operation word; args is the
    // verb-excluded tail.
    let a = BlockPipeArgs::try_parse_from(args)
        .map_err(|e| crate::cli_args::refusal_line("fno mux block pipe", &e))?;
    if a.session.is_some() {
        note_server_flag("--session");
    }
    Ok(ParsedBlockPipe {
        session: a.server.or(a.session),
        json: a.json,
        from: parse_u64(
            a.from.as_deref().ok_or("block pipe needs --from <pane>")?,
            "--from",
        )?,
        to: parse_u64(
            a.to.as_deref().ok_or("block pipe needs --to <pane>")?,
            "--to",
        )?,
        block: match &a.block {
            Some(b) => parse_block_sel(b)?,
            None => BlockSel::Last,
        },
        force: a.force,
    })
}

/// A parsed `block annotate` invocation. Pure-parse struct, mirrors
/// [`ParsedBlockPipe`]. `node` carries the backlog node the finding is scoped
/// to (the caller supplies the pane's server-tracked `FNO_NODE`, surfaced to
/// the mux client as `Layout::focus_node`); the porcelain never guesses it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParsedBlockAnnotate {
    pub(super) session: Option<String>,
    pub(super) from: u64,
    pub(super) block: BlockSel,
    pub(super) node: String,
    pub(super) message: String,
}

/// Parse the tokens after `mux block annotate` into a [`ParsedBlockAnnotate`].
/// Pure, so the grammar is unit-testable without a socket. `--node` and `-m`
/// are required; a missing `--node` is the "specify the node" refusal (a
/// non-agent pane has no provenance to resolve, so the caller must name it).
pub(super) fn parse_block_annotate(args: &[OsString]) -> Result<ParsedBlockAnnotate, String> {
    // The typed tree already routed on the operation word; args is the
    // verb-excluded tail.
    let a = BlockAnnotateArgs::try_parse_from(args)
        .map_err(|e| crate::cli_args::refusal_line("fno mux block annotate", &e))?;
    if a.session.is_some() {
        note_server_flag("--session");
    }
    let session = a.server.or(a.session);
    let from = a
        .from
        .as_deref()
        .map(|v| parse_u64(v, "--from"))
        .transpose()?;
    let block = match &a.block {
        Some(b) => parse_block_sel(b)?,
        None => BlockSel::Last,
    };
    let node = a.node;
    let message = a.message.ok_or("block annotate needs -m <text>")?;
    if message.trim().is_empty() {
        return Err("block annotate: --message is empty".to_string());
    }
    Ok(ParsedBlockAnnotate {
        session,
        from: from.ok_or("block annotate needs --from <pane>")?,
        block,
        node: node.ok_or(
            "block annotate needs --node <id> (a pane's node cannot be guessed; \
             pass the node whose work this pane holds)",
        )?,
        message,
    })
}

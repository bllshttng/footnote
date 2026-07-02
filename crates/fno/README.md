# fno

`cargo install fno` lands the fno terminal front door: a native terminal
multiplexer plus the bootstrapper that provisions and forwards to the fno
Python CLI.

## The mux

Run `fno` bare on a TTY and you get an interactive `$SHELL` in a persistent
session: a background server owns the PTY and its emulated screen, and the
client you are looking at is a thin compositor attached over a Unix socket at
`~/.fno/mux/<session>.sock` (dir `0700`). Quit the client (`Ctrl-\`), close
the terminal, or `kill -9` it - the shell keeps running, and the next `fno`
reattaches to the exact same screen (alt-screen programs included).

```sh
fno                      # attach to the "main" session, spawning its server if absent
fno mux server           # run a session server explicitly (scriptable)
fno mux server --session work
```

Phase-1 scope: one full-window pane, one client at a time. Input is never
dropped; render frames are droppable (a slow client just skips to the newest
self-contained frame). The server survives `cargo install` upgrades and
refuses version-skewed clients with both versions named.

## The CLI (bootstrapper)

Any `fno <subcommand>` invocation forwards to the real `fno` CLI (the Python
wheel, which bundles the `fno-agents*` binaries). On first run the shim
provisions it via [uv](https://docs.astral.sh/uv/), verifies the package is
this project's, and then `exec`s it; later runs forward instantly.

```sh
cargo install fno
fno --version          # first run provisions uv + the wheel, then runs
```

The supported cargo front door is `cargo install fno`. The sibling crate
`fno-agents` is published for name-reservation and to back this crate;
`cargo install fno-agents` (pure Rust, no Python CLI) is not an advertised
install path.

Pre-publish testing: set `FNO_BOOTSTRAP_WHEEL=/path/to/fno-*.whl` to provision
from a local wheel instead of resolving `fno` by name on PyPI.

Apache-2.0.

# Distribution

How the `fno-agents` Rust supervisor reaches users, and how the Python `fno`
package finds it. Authored in Phase 6 W6.

> **Status: machinery only.** Every workflow below is gated and nothing has been
> published yet. The live-publish steps are deliberately manual and maintainer-only,
> listed at the end of this doc. Reserving names and cutting releases is a
> deliberate act, not a side effect of merging.

## Three artifacts, one crate source

| Artifact | Built by | Installed by | Audience |
|----------|----------|--------------|----------|
| Platform wheel (binaries bundled) | `release-wheels.yml` (cibuildwheel-style matrix) | `pip install fno` / `uv tool install fno` | Python users of the `fno` CLI |
| Standalone binary tarball | `release-binaries.yml` | `gh release download` / unpack to `~/bin` | non-Python users |
| `fno-agents` crate | `crates-publish.yml` | `cargo install fno-agents` / `cargo add fno-agents` | other Rust projects |

Target platforms: darwin-arm64, darwin-x64, linux-x64, linux-arm64. Windows is
**built but not released** this phase (POSIX `flock` and Unix-socket IPC differ;
the named-pipe port is a separate spec). Its matrix row stays so the toolchain is
exercised for the Phase 7+ deferral.

### Crate names

`fno-agents` (crates.io) and `fno` (PyPI) are the real publish targets. They live in
**separate registries** and do not collide with each other. The Rust supervisor crate
stays named `fno-agents` (not `fno`): naming a Rust binary `fno` would collide on PATH
with the Python `fno` console script. (`fno` on crates.io is reserved separately as a
thin bootstrapper crate; see the install-channel notes.)

## How `pip install fno` lands the Rust binaries on PATH

The `fno` wheel is normally pure-Python (`py3-none-any`). The release CI builds the
Rust crate first (`cargo build --bins`), stages **all three** binaries
(`fno-agents`, `fno-agents-daemon`, `fno-agents-worker`) under `cli/src/fno/_bin/`,
then builds the wheel. A hatchling build hook (`cli/hatch_build.py`) detects the
staged binaries and:

- ships each as a wheel **script** (`*.data/scripts/<binary>`), which pip installs into
  the environment's `bin/` (`Scripts/` on Windows) — i.e. on PATH, so daemon-backed
  verbs work with no second install (US6); and
- tags the wheel `py3-none-<platform>` rather than `cp3XX-cp3XX-<platform>`, because a
  standalone executable is interpreter-agnostic. One wheel serves every Python 3.x on
  the platform, so there is no per-version build explosion and no sdist fallback on
  newer Pythons.

Staging is **all-or-nothing**: zero binaries staged → a pure-Python wheel (a valid
variant); a partial set (1–2 of 3) **hard-fails** the build — a release wheel must be
binary-complete, so a staging defect never reaches PyPI. The same hook also
force-includes the events schema and the **LICENSE + NOTICE** texts (under
`fno/_licenses/`), each required: a schema-less or license-less wheel hard-fails rather
than ship silently (US5). The CI runs a clean-machine smoke
(`cli/tests/smoke/clean_machine_smoke.sh`) against the freshly built wheel — all three
binaries on PATH, the internalized verbs run, the clone-only verbs degrade — failing the
release on any miss.

When no binary is staged (local dev, `pip install -e`, sdist) the hook no-ops the binary
bundling and the ordinary pure-Python wheel builds — **the Python package never requires
a Rust toolchain to build.** On Linux the CI sets
`FNO_AGENTS_WHEEL_PLATFORM=manylinux_2_17_*` because PyPI rejects plain `linux_*` tags and
`auditwheel` cannot repair a wheel whose only native payload is a scripts-dir executable
(not a `.so`); the binaries are built with the musl target so the manylinux claim holds.

## Rust runtime for `fno agents` (default)

The Rust daemon is the **default** runtime for the daemon-native verbs and four of the five
shared verbs (which closed the flag +
stdout parity gap for `list`/`stop`/`rm`/`reconcile`). By default `fno agents <verb>` execs
the installed `fno-agents` binary for every verb the Rust client implements at parity, and
uses the mature Python dispatch for `ask` and the Python-only verbs. `FNO_AGENTS_RUNTIME`
selects the runtime explicitly:

| `FNO_AGENTS_RUNTIME` | Behavior |
|---|---|
| unset / anything else | **`auto`** (default): exec the installed binary for the daemon-native + the four cut-over shared verbs; Python for `ask` and the Python-only verbs |
| `rust` | force the binary for every verb, `ask` included (missing binary → exit 127) |
| `python` | force the Python dispatch; never touch the binary |

```bash
fno agents spawn worker-A --provider codex             # Rust by default when installed
fno agents list --json                                 # Rust: render_json shape; table on a TTY
fno agents stop worker-A                               # Rust: prints "stopped: worker-A (<short_id>)"
fno agents ask worker-A "hello" --provider codex       # Python (ask stays on Python — see below)
FNO_AGENTS_RUNTIME=rust fno agents ask worker-A "..."  # force the binary for every verb, ask included
```

Why the default routes most verbs, and is install-aware:

- **Daemon-native + four shared verbs auto-route; `ask` stays on Python.** The Rust client
  implements `spawn`, `ask`, `list`, `status`, `stop`, `rm`, `reconcile`, `drive`, and the
  `*-channel` verbs. Four shared verbs reached flag + stdout parity:
  `stop` prints `stopped: <name> (<short_id>)`, `rm` prints `removed: <name>`, and
  `list`/`reconcile` emit the `render_json` shape under `--json`/non-TTY (a functional
  table on a TTY) and accept `--cwd/--provider/--status`. **`ask` is NOT auto-routed**: the
  daemon only PTY-manages codex/gemini (`ClaudeProvider.as_pty()` is `None` — claude is a
  `claude --bg` shellout), so routing a claude `ask` hits "worker not reachable" and the
  no-signal detector spins to the timeout; and even for codex/gemini the daemon's reply is
  the whole settled TUI screen, not Python's extracted model reply. The daemon's
  create-on-first-contact `handle_ask` groundwork (auto-spawn via
  `ProviderWithPty::create_argv`, readiness-poll reply) is in place for
  `FNO_AGENTS_RUNTIME=rust` force-mode and a future reply-extraction follow-up, but `auto`
  keeps `ask` on Python. Only `ask` and the Python-only verbs (`logs`, `ping`,
  `drive-authority`, `attach`, `resume`, `trace`) stay on Python. (One decided behavior,
  not a gap: `list` reports the daemon's PTY-worker status and deliberately does not
  replicate Python's per-row `claude agents --json` `live_status` augmentation. Under the
  replace architecture the daemon is the sole backend, so PTY-worker/registry status is
  the canonical liveness signal; `live_status` stays present-but-null for shape parity.) Forcing
  `FNO_AGENTS_RUNTIME=python` pins everything to the Python dispatch. Tests parse
  `client.rs` and introspect the Typer
  commands so this split can't drift silently.
- **Install-aware resolution.** `auto` resolves *installed* binaries only — bundled wheel
  dir → launcher sibling → `PATH` — and deliberately ignores the cargo dev target. So a
  development checkout (where only `crates/fno-agents/target/release/` exists) stays on
  Python by default; opt the local build in with `FNO_AGENTS_RUNTIME=rust`. The forced
  `rust` path uses the full resolution order (bundled → sibling → `PATH` → cargo dev) and
  exits 127 with an actionable message when no binary is found.

`fno agents --help` always stays on the Python group help so the wrapper stays
discoverable; because the shared verbs stay on Python, `fno agents <verb> --help` renders
the Python help for them. (Per-verb help for the daemon-native verbs is pending Rust-client
support.)

## Live-publish runbook (gated, maintainer-only)

None of these run automatically. A maintainer performs them deliberately.

- [ ] Publish `fno-agents` on crates.io and `fno` on PyPI over the reserved `0.0.0` placeholders (US7's by-name install and the cargo channel both depend on the real `fno` being published)
- [ ] Set repo secrets `CARGO_REGISTRY_TOKEN` (crates.io) and `PYPI_API_TOKEN` (PyPI)
- [ ] Cut a release: `git tag vX.Y.Z && git push origin vX.Y.Z` — builds wheels + binaries and attaches binaries to the GitHub Release (no PyPI/crates upload yet)
- [ ] Publish wheels to PyPI: re-run `release-wheels` via workflow_dispatch with `publish=true`
- [ ] Publish the crate: run `crates-publish` via workflow_dispatch with `confirm=true` (permanent; a version cannot be re-uploaded)
- [ ] fno.sh channel: wire DNS for `fno.sh` to Cloudflare with CF-managed TLS, and deploy `scripts/install/fno.sh` to `https://fno.sh` (Cloudflare Pages git-integration or a `wrangler` CI step) served as `text/plain` with purge-on-deploy. The one-liner ends in `uv tool install fno` by name, so it also depends on the PyPI publish above.
- [ ] brew channel (depends on the PyPI publish above): create the own tap repo `github.com/<owner>/homebrew-fno` and copy `scripts/install/homebrew/fno.rb` into it as `Formula/fno.rb`. Fill the placeholder `url` + `sha256` with the published per-arch macOS wheels (arm64 + x86_64, in lockstep - a `url` bump without its `sha256` is caught by `brew audit`/install). Deps install from PyPI at install time (own-tap formulae install with network); vendoring them as offline `resource` blocks (`brew update-python-resources fno`) is optional and only needed for a future homebrew-core submission. Verify with `brew audit --strict --new fno` + `brew install <owner>/fno/fno` + `brew test fno` on a clean macOS host. On every later release, re-bump `url`+`sha256` together. **Move the tap by GitHub repo transfer + redirect, never by delete** - deleting the old repo breaks the redirect and every installed user (`brew update`/`brew upgrade` resolve through GitHub's 301).

Before the first real publish, bump versions in lockstep: `crates/fno-agents/Cargo.toml`
and `cli/pyproject.toml` are independent `0.1.0` today.

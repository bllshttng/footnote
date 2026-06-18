# fno (cargo bootstrapper)

`cargo install fno` lands a small Rust shim. On first run it provisions the
real `fno` CLI (the Python wheel, which bundles the `fno-agents*` binaries) via
[uv](https://docs.astral.sh/uv/), verifies the package is this project's, and
then forwards every invocation to it. The CLI itself lives in the `fno` package
on PyPI; this crate is only the cargo front door.

```sh
cargo install fno
fno --version          # first run provisions uv + the wheel, then runs
```

The supported cargo front door is `cargo install fno`. The sibling crate
`fno-agents` is published for name-reservation and to back this bootstrapper;
`cargo install fno-agents` (pure Rust, no Python CLI) is not an advertised
install path.

Pre-publish testing: set `FNO_BOOTSTRAP_WHEEL=/path/to/fno-*.whl` to provision
from a local wheel instead of resolving `fno` by name on PyPI.

Apache-2.0.

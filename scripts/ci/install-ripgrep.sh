#!/usr/bin/env bash
# Install a pinned, checksummed static ripgrep from its GitHub release.
# `apt-get update && apt-get install ripgrep` hung 28 minutes on the hosted
# runner on 2026-10-07 and timed a cargo shard out before any test ran. A
# release download has no package index or mirror to stall on.
set -euo pipefail

version=15.2.0
sha256=33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c
name="ripgrep-${version}-x86_64-unknown-linux-musl"
tmp=$(mktemp -d)

curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 \
  -o "$tmp/rg.tar.gz" \
  "https://github.com/BurntSushi/ripgrep/releases/download/${version}/${name}.tar.gz"
echo "${sha256}  $tmp/rg.tar.gz" | sha256sum -c -
tar -xzf "$tmp/rg.tar.gz" -C "$tmp"
sudo install -m 0755 "$tmp/${name}/rg" /usr/local/bin/rg
rg --version

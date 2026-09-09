#!/usr/bin/env bash
set -euo pipefail

uv run --project cli fno doctor lint graph-parity

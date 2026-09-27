#!/usr/bin/env bash
set -euo pipefail

bash tests/spec/test_blueprint_phase_close.sh
bash tests/spec/test_claims_arg.sh
bash tests/spec/test_executor_transcription.sh
uv run --project cli fno-py doctor test --stream \
  tests/spec/test_product_md_check.py \
  tests/spec/test_impeccable_stages_validator.py

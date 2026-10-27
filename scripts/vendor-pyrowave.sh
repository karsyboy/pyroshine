#!/usr/bin/env bash
# Regenerate, verify (--check), or test (--test) the pinned PyroWave patch series.
# Optional --source PATH uses a local upstream checkout. See vendor/README.md.
set -euo pipefail
exec bash "$(dirname "${BASH_SOURCE[0]}")/vendor-dependency.sh" pyrowave "$@"

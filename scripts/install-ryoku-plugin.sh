#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Author in the Ryoku scaffold directory, never an installed copy.
documents="$(xdg-user-dir DOCUMENTS)"
source_dir="${1:-$documents/ryoku-plugins/linet}"
python3 scripts/install-ryoku-plugin.py "$source_dir"

#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'PYTHON'
import importlib.util, os
from pathlib import Path
spec=importlib.util.spec_from_file_location("install_user", "scripts/install-user.py")
m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
home=Path.home()
config=Path(os.environ.get("XDG_CONFIG_HOME", str(home/".config")))
data=Path(os.environ.get("XDG_DATA_HOME", str(home/".local/share")))
m.install_desktop(Path(os.environ.get("LINETCTL", str(home/".local/bin/linetctl"))),data)
PYTHON

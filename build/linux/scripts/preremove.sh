#!/bin/sh
# preremove — runs as root before files are deleted.
# Removes the statusline configuration so the user's Claude CLI doesn't keep
# trying to invoke a node command they no longer expect.

set -u

if [ -z "${SUDO_USER:-}" ] || [ "$SUDO_USER" = "root" ]; then
    exit 0
fi

TARGET_USER="$SUDO_USER"
USER_HOME=$(getent passwd "$TARGET_USER" | cut -d: -f6)
if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
    exit 0
fi

SETTINGS_PATH="$USER_HOME/.claude/settings.json"
PYTHON=$(command -v python3 || command -v python)
if [ -z "$PYTHON" ] || [ ! -f "$SETTINGS_PATH" ]; then
    exit 0
fi

sudo -u "$TARGET_USER" "$PYTHON" - "$SETTINGS_PATH" uninstall <<'PYEOF'
import json, os, sys, tempfile

path = sys.argv[1]

if not os.path.exists(path):
    sys.exit(0)

try:
    with open(path, "r") as f:
        data = f.read().strip()
        settings = json.loads(data) if data else {}
    if not isinstance(settings, dict):
        sys.exit(0)
except json.JSONDecodeError:
    sys.exit(0)

sl = settings.get("statusLine")
if isinstance(sl, dict) and "rate_limits.json" in str(sl.get("command", "")):
    settings.pop("statusLine", None)
settings.pop("statuslineCommand", None)

tmp_fd, tmp_path = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".settings.", suffix=".tmp")
try:
    with os.fdopen(tmp_fd, "w") as f:
        json.dump(settings, f, indent=2)
    os.replace(tmp_path, path)
except Exception:
    try:
        os.unlink(tmp_path)
    except OSError:
        pass
    raise
PYEOF

exit 0

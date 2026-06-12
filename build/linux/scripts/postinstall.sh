#!/bin/sh
# postinstall — runs as root after dpkg/rpm places files.
# Configures the Claude statusline for the invoking user via $SUDO_USER.
# AppImage installs use a separate first-run check inside the app.

set -u

if [ -z "${SUDO_USER:-}" ] || [ "$SUDO_USER" = "root" ]; then
    echo "ClawdPanel postinstall: SUDO_USER not set; skipping statusline configuration."
    echo "  Run \`clawdpanel --configure-statusline\` manually after first launch."
    exit 0
fi

TARGET_USER="$SUDO_USER"
USER_HOME=$(getent passwd "$TARGET_USER" | cut -d: -f6)
if [ -z "$USER_HOME" ] || [ ! -d "$USER_HOME" ]; then
    echo "ClawdPanel postinstall: could not resolve home dir for $TARGET_USER; skipping."
    exit 0
fi

SETTINGS_PATH="$USER_HOME/.claude/settings.json"
PYTHON=$(command -v python3 || command -v python)
if [ -z "$PYTHON" ]; then
    echo "ClawdPanel postinstall: python not found; skipping statusline configuration."
    exit 0
fi

sudo -u "$TARGET_USER" "$PYTHON" - "$SETTINGS_PATH" install <<'PYEOF'
import json, os, sys, tempfile

path = sys.argv[1]
mode = sys.argv[2]

STATUSLINE_CMD = (
    "node -e \"const fs=require('fs');const p=require('path');const os=require('os');"
    "const d=fs.readFileSync(0,'utf-8');if(d){const parsed=JSON.parse(d);"
    "fs.writeFileSync(p.join(os.homedir(),'.claude','rate_limits.json'),"
    "JSON.stringify({...parsed,captured_at:Date.now()}))}\""
)

os.makedirs(os.path.dirname(path), exist_ok=True)

settings = {}
if os.path.exists(path):
    try:
        with open(path, "r") as f:
            data = f.read().strip()
            settings = json.loads(data) if data else {}
        if not isinstance(settings, dict):
            print(f"ClawdPanel: {path} is not a JSON object; skipping.", file=sys.stderr)
            sys.exit(0)
    except json.JSONDecodeError as e:
        print(f"ClawdPanel: {path} is not valid JSON ({e}); skipping.", file=sys.stderr)
        sys.exit(0)

if mode == "install":
    # "statusLine" is the key Claude Code actually reads; older installs wrote
    # the nonexistent "statuslineCommand" (cleaned up on both paths).
    settings["statusLine"] = {"type": "command", "command": STATUSLINE_CMD}
    settings.pop("statuslineCommand", None)
elif mode == "uninstall":
    sl = settings.get("statusLine")
    if isinstance(sl, dict) and "rate_limits.json" in str(sl.get("command", "")):
        settings.pop("statusLine", None)
    settings.pop("statuslineCommand", None)

tmp_fd, tmp_path = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".settings.", suffix=".tmp")
try:
    with os.fdopen(tmp_fd, "w") as f:
        json.dump(settings, f, indent=2)
    os.replace(tmp_path, path)
    print(f"ClawdPanel: statusline {mode} complete ({path})")
except Exception:
    try:
        os.unlink(tmp_path)
    except OSError:
        pass
    raise
PYEOF

exit 0

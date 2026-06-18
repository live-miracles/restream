#!/usr/bin/env bash
# Reset the dashboard password to 'admin'.
# Usage:
#   sudo bash /opt/restream/scripts/server-reset-password.sh
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
    echo "ERROR: run as root (sudo bash scripts/server-reset-password.sh)" >&2
    exit 1
fi

DB_PATH="${DB_PATH:-/var/lib/restream/data.db}"
APP_DIR="${APP_DIR:-/opt/restream}"

cd "$APP_DIR"
node -e "const db=require('better-sqlite3')('${DB_PATH}'); db.prepare(\"DELETE FROM meta WHERE key='dashboardPasswordHash'\").run()"
systemctl restart restream.service

echo "Password reset to 'admin'. Change it in Settings after logging in."

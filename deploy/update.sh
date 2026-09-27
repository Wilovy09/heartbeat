#!/usr/bin/env bash
# Installs a Heartbeat release built by .github/workflows/release.yml, in place: the
# binary (templates, static files and libs are embedded in it since v0.2.0) and deploy/
# are replaced; .env is never touched. The previous binary is kept as
# target/release/heartbeat.prev. Picks the x86_64 or aarch64 build to match this machine.
#
# The database is backed up first (data/backups/, the latest 5 are kept). Coming from
# 0.2 (data in JSON files), the files are archived to data/backups/ and imported with
# `heartbeat migrate`; --rollback to a 0.2 binary then puts them back.
#
# Usage (from the install directory, e.g. /arena/heartbeat):
#   HEARTBEAT_REPO=owner/heartbeat deploy/update.sh            # latest release
#   HEARTBEAT_REPO=owner/heartbeat deploy/update.sh v1.2.0     # a specific tag
#   deploy/update.sh --rollback                                 # back to the previous binary
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PM2_NAME="${PM2_NAME:-heartbeat}"
BIN="$APP_DIR/target/release/heartbeat"
cd "$APP_DIR"

# A value from .env, or the default.
env_value() {
  local value
  value="$(grep -E "^$1=" .env 2>/dev/null | tail -1 | cut -d= -f2-)"
  echo "${value:-$2}"
}
DB="$(env_value DATABASE_PATH ./data/heartbeat.db)"
BACKUPS="$APP_DIR/data/backups"
KEEP_BACKUPS=5
# Written when this script imports a 0.2 install, so a rollback knows to undo it.
MIGRATED_MARK="$APP_DIR/data/.migrated-from-0.2"
# Heartbeat 0.2's files, as `heartbeat migrate` finds them.
LEGACY=(
  "$(env_value APPS_FILE ./data/apps.json)"
  "$(env_value UPTIME_DIR ./data/uptime)"
  "$(env_value SESSIONS_FILE ./data/sessions.json)"
  "$(env_value NOTICES_FILE ./data/notices.json)"
  "$(env_value ALERT_TEMPLATES_FILE ./data/alert_templates.json)"
  "$(env_value THEME_FILE ./data/theme.css)"
)

stamp() { date -u +%Y%m%dT%H%M%SZ; }

# Backs the database up with the installed binary (it knows its own schema), keeping
# the latest $KEEP_BACKUPS. Nothing to do before the first 0.3 install.
backup_db() {
  [[ -f "$DB" && -x "$BIN" ]] || return 0
  mkdir -p "$BACKUPS"
  "$BIN" backup "$BACKUPS/heartbeat-$(stamp).db"
  ls -1t "$BACKUPS"/heartbeat-*.db 2>/dev/null | tail -n +$((KEEP_BACKUPS + 1)) | xargs -r rm -f
}

restart() {
  pm2 restart "$PM2_NAME" >/dev/null
  sleep 3
  local port
  port="$(grep -E '^PORT=' .env 2>/dev/null | cut -d= -f2)"
  if curl -fsS "http://localhost:${port:-8090}/healthz" >/dev/null; then
    echo "OK: $PM2_NAME is up"
  else
    echo "WARNING: /healthz did not answer; check 'pm2 logs $PM2_NAME'" >&2
    exit 1
  fi
}

if [[ "${1:-}" == "--rollback" ]]; then
  [[ -f "$BIN.prev" ]] || { echo "No previous binary to roll back to." >&2; exit 1; }
  mv "$BIN.prev" "$BIN"
  echo "Rolled back the binary."
  if [[ -f "$MIGRATED_MARK" ]]; then
    # Back to 0.2, which reads the files: put them back where `migrate` found them.
    # Whatever changed since the import (checks, edits) exists only in the database.
    pm2 stop "$PM2_NAME" >/dev/null 2>&1 || true
    for path in "${LEGACY[@]}"; do
      if [[ -e "$path.migrated" && ! -e "$path" ]]; then mv "$path.migrated" "$path"; fi
    done
    mv "$DB" "$DB.rolled-back-$(stamp)"
    rm -f "$DB-wal" "$DB-shm" "$MIGRATED_MARK"
    echo "Restored the 0.2 data files; the database was set aside as $DB.rolled-back-*."
  fi
  restart
  exit 0
fi

REPO="${HEARTBEAT_REPO:?set HEARTBEAT_REPO=owner/repo}"
TAG="${1:-latest}"
if [[ "$TAG" == "latest" ]]; then
  TAG="$(curl -fsS "https://api.github.com/repos/$REPO/releases/latest" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
fi
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64 | aarch64) ;;
  arm64) ARCH=aarch64 ;;
  *) echo "No release for $ARCH" >&2; exit 1 ;;
esac
NAME="heartbeat-$TAG-$ARCH-linux"
URL="https://github.com/$REPO/releases/download/$TAG/$NAME.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading $TAG..."
curl -fsSL -o "$TMP/$NAME.tar.gz" "$URL"
curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$URL.sha256"
# Compare the hash only: the checksum file's filename column isn't trusted (v0.1.0's
# carries a "dist/" prefix).
expected="$(cut -d' ' -f1 "$TMP/$NAME.tar.gz.sha256")"
actual="$(sha256sum "$TMP/$NAME.tar.gz" | cut -d' ' -f1)"
[[ -n "$expected" && "$expected" == "$actual" ]] || { echo "Checksum mismatch for $NAME.tar.gz" >&2; exit 1; }
echo "Checksum OK"
tar -C "$TMP" -xzf "$TMP/$NAME.tar.gz"

backup_db
# The binary replaced now is a 0.3 one: a later rollback no longer goes back to 0.2.
if [[ -f "$DB" ]]; then rm -f "$MIGRATED_MARK"; fi

mkdir -p target/release
[[ -f "$BIN" ]] && cp "$BIN" "$BIN.prev"
install -m 755 "$TMP/$NAME/target/release/heartbeat" "$BIN"
# Releases before v0.2.0 also shipped templates/, static/ and libs/ next to the binary.
for dir in templates static libs deploy; do
  [[ -d "$TMP/$NAME/$dir" ]] || continue
  rm -rf "$APP_DIR/$dir"
  cp -r "$TMP/$NAME/$dir" "$APP_DIR/$dir"
done
cp "$TMP/$NAME/.env.example" "$APP_DIR/.env.example"
echo "Installed $TAG. Compare .env with .env.example for new variables."

# Coming from 0.2: archive its files and import them (the new binary won't start until
# they are). `migrate` only renames the originals, and the archive is kept regardless.
if [[ ! -f "$DB" && -e "${LEGACY[0]}" ]]; then
  echo "Found Heartbeat 0.2 data: importing it into $DB."
  mkdir -p "$BACKUPS"
  existing=()
  for path in "${LEGACY[@]}"; do
    if [[ -e "$path" ]]; then existing+=("$path"); fi
  done
  tar -czf "$BACKUPS/data-0.2-$(stamp).tar.gz" "${existing[@]}"
  pm2 stop "$PM2_NAME" >/dev/null 2>&1 || true
  if ! "$BIN" migrate; then
    echo "The import failed and nothing was changed; 'deploy/update.sh --rollback' restores the previous version." >&2
    exit 1
  fi
  touch "$MIGRATED_MARK"
fi
restart

#!/usr/bin/env bash
# Installs a Heartbeat release built by .github/workflows/release.yml, in place:
# the binary, templates, static files and vendored libs are replaced; .env and data/ are
# never touched. The previous binary is kept as target/release/heartbeat.prev.
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
  echo "Rolled back the binary (templates/static stay at the newer version)."
  restart
  exit 0
fi

REPO="${HEARTBEAT_REPO:?set HEARTBEAT_REPO=owner/repo}"
TAG="${1:-latest}"
if [[ "$TAG" == "latest" ]]; then
  TAG="$(curl -fsS "https://api.github.com/repos/$REPO/releases/latest" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
fi
NAME="heartbeat-$TAG-x86_64-linux"
URL="https://github.com/$REPO/releases/download/$TAG/$NAME.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading $TAG..."
curl -fsSL -o "$TMP/$NAME.tar.gz" "$URL"
curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$URL.sha256"
(cd "$TMP" && sha256sum -c "$NAME.tar.gz.sha256")
tar -C "$TMP" -xzf "$TMP/$NAME.tar.gz"

mkdir -p target/release
[[ -f "$BIN" ]] && cp "$BIN" "$BIN.prev"
install -m 755 "$TMP/$NAME/target/release/heartbeat" "$BIN"
for dir in templates static libs deploy; do
  rm -rf "$APP_DIR/$dir"
  cp -r "$TMP/$NAME/$dir" "$APP_DIR/$dir"
done
cp "$TMP/$NAME/.env.example" "$APP_DIR/.env.example"
echo "Installed $TAG. Compare .env with .env.example for new variables."
restart

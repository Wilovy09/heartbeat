demo:
    APPS_FILE=./data/demo/apps.json UPTIME_DIR=./data/demo/uptime COOKIE_SECURE=false \
    LOGIN_URL="${LOGIN_URL:-http://127.0.0.1:8097/login}" \
    ALLOWED_HOSTS="${ALLOWED_HOSTS:?set ALLOWED_HOSTS, e.g. *.example.com}" cargo run

run:
    cargo run

fmt:
    cargo fmt --all

check:
    cargo fmt --all -- --check
    RUSTC_WRAPPER= cargo clippy --all-targets -- -W clippy::pedantic -D warnings
    cargo test

e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q
    dir="$(mktemp -d)"
    hash="$(echo smoke-test-password | ./target/debug/heartbeat hash-password 2>/dev/null)"
    PORT=8199 COOKIE_SECURE=false AUTH_MODE=password ADMIN_EMAIL=admin@example.com \
      ADMIN_PASSWORD_HASH="$hash" ALLOWED_HOSTS="*.example.com" \
      APPS_FILE="$dir/apps.json" UPTIME_DIR="$dir/uptime" SESSIONS_FILE="$dir/sessions.json" \
      ./target/debug/heartbeat > "$dir/server.log" 2>&1 &
    pid=$!
    trap 'kill $pid; rm -rf "$dir"' EXIT
    for _ in $(seq 50); do curl -fsS localhost:8199/healthz >/dev/null 2>&1 && break; sleep 0.2; done
    cd tests/e2e && npm install --silent && node smoke.mjs

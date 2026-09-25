# Local demo: fake data regenerated every run (scripts/demo_data.py) and a local admin,
# no login server needed. Sign in at http://localhost:8090 as demo@example.com / demo.
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    python3 scripts/demo_data.py
    cargo build -q
    hash="$(echo demo | ./target/debug/heartbeat hash-password 2>/dev/null)"
    echo "Heartbeat demo: http://localhost:8090  (demo@example.com / demo)"
    AUTH_MODE=password ADMIN_EMAIL=demo@example.com ADMIN_PASSWORD_HASH="$hash" \
      ALLOWED_HOSTS="httpbin.org,*.invalid" COOKIE_SECURE=false \
      APPS_FILE=./data/demo/apps.json UPTIME_DIR=./data/demo/uptime \
      SESSIONS_FILE=./data/demo/sessions.json ./target/debug/heartbeat

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

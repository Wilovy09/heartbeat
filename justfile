# Local demo (`just demo` in Spanish, `just demo en` in English): fake data regenerated
# every run (scripts/demo_data.py) and a local admin,
# no login server needed. Sign in at http://localhost:8090 as demo@example.com / demo
# (admin) or viewer@example.com / demo (read-only). Reminders every 2 min, /metrics with
# "Authorization: Bearer demo". Built with the `demo` feature: the apps' logs are generated
# (src/demo.rs), since *.heartbeat.invalid never resolves.
# Alerts go to httpbin.org/post (a public echo endpoint), so /settings can ping and send
# test alerts without a Slack workspace.
demo lang="es":
    #!/usr/bin/env bash
    set -euo pipefail
    export APP_LANG="{{lang}}"
    python3 scripts/demo_data.py
    cargo build -q --features demo
    hash="$(echo demo | ./target/debug/heartbeat hash-password 2>/dev/null)"
    export AUTH_MODE=password ADMIN_EMAIL=demo@example.com ADMIN_PASSWORD_HASH="$hash" \
      VIEWER_EMAIL=viewer@example.com VIEWER_PASSWORD_HASH="$hash" ALERT_REMIND_MINS=2 \
      ALLOWED_HOSTS="httpbin.org,*.invalid" COOKIE_SECURE=false \
      DATABASE_PATH=./data/demo/heartbeat.db \
      APPS_FILE=./data/demo/apps.json UPTIME_DIR=./data/demo/uptime \
      SESSIONS_FILE=./data/demo/sessions.json ALERT_TEMPLATES_FILE=./data/demo/alert_templates.json \
      NOTICES_FILE=./data/demo/notices.json THEME_FILE=./data/demo/theme.css METRICS_TOKEN=demo \
      ALERT_WEBHOOK_URLS=https://httpbin.org/post ALERT_MENTIONS="here,U0123DEMO" \
      PUBLIC_URL=http://localhost:8090
    # The demo data is written as a 0.2 install, so every demo run exercises the importer.
    rm -rf ./data/demo/heartbeat.db ./data/demo/heartbeat.db-wal ./data/demo/heartbeat.db-shm ./data/demo/*.migrated*
    ./target/debug/heartbeat migrate
    echo "Heartbeat demo: http://localhost:8090  (demo@example.com / demo, viewer@example.com / demo)"
    ./target/debug/heartbeat

run:
    cargo run

fmt:
    cargo fmt --all

check:
    cargo fmt --all -- --check
    cargo deny check
    RUSTC_WRAPPER= cargo clippy --all-targets --all-features -- -W clippy::pedantic -D warnings
    cargo test --all-features

e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q
    dir="$(mktemp -d)"
    hash="$(echo smoke-test-password | ./target/debug/heartbeat hash-password 2>/dev/null)"
    PORT=8199 COOKIE_SECURE=false AUTH_MODE=password ADMIN_EMAIL=admin@example.com \
      ADMIN_PASSWORD_HASH="$hash" ALLOWED_HOSTS="*.example.com" \
      DATABASE_PATH="$dir/heartbeat.db" APPS_FILE="$dir/apps.json" UPTIME_DIR="$dir/uptime" SESSIONS_FILE="$dir/sessions.json" \
      NOTICES_FILE="$dir/notices.json" ALERT_TEMPLATES_FILE="$dir/alert_templates.json" THEME_FILE="$dir/theme.css" \
      ./target/debug/heartbeat > "$dir/server.log" 2>&1 &
    pid=$!
    trap 'kill $pid; rm -rf "$dir"' EXIT
    for _ in $(seq 50); do curl -fsS localhost:8199/healthz >/dev/null 2>&1 && break; sleep 0.2; done
    cd tests/e2e && npm install --silent && node smoke.mjs

# Benchmarks (needs k6): a release build against a synthetic database of `apps` apps with
# `days` days of checks, loaded endpoint by endpoint. Prints req/s and latency per endpoint,
# startup time and memory, and saves the run to data/bench/results/. VUS (clients, 20) and
# DURATION (per endpoint, 15s) tune the load: `VUS=50 just bench 500`.
bench apps="100" days="30":
    scripts/bench.sh {{apps}} {{days}}

# Side by side: the latest run of each size, or the result files given, oldest first.
bench-compare *files:
    python3 scripts/bench_compare.py {{files}}

# The landing page and docs (site/), built from README.md / README_ES.md and served on
# http://localhost:8200. Deployed to GitHub Pages by .github/workflows/pages.yml.
site:
    uv run site/build.py --serve

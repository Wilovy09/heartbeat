#!/usr/bin/env bash
# `just bench [apps] [days]`: a release build against a synthetic database
# (scripts/bench_seed.py), loaded endpoint by endpoint with k6 (bench/load.js).
# Reports startup time, memory (idle and peak RSS) and each endpoint's req/s and
# latency, and saves it all to data/bench/results/ to compare runs.
#
# Seeds are cached per size in data/bench/seed-<apps>x<days>.db (delete one to reseed);
# each run works on a fresh copy. VUS and DURATION pass through to k6. BENCH_BIN runs
# another build instead of this tree's (e.g. a worktree of the previous release), to
# compare the two on the same data.
set -euo pipefail

APPS="${1:-100}"
DAYS="${2:-30}"
PORT="${BENCH_PORT:-8199}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/data/bench"
SEED="$DIR/seed-${APPS}x${DAYS}.db"
RUN="$DIR/run"
BIN="${BENCH_BIN:-$ROOT/target/release/heartbeat}"
cd "$ROOT"

command -v k6 >/dev/null || { echo "k6 is needed: brew install k6 (or see k6.io)"; exit 1; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
rss_mb() { ps -o rss= -p "$1" | awk '{ printf "%.1f", $1 / 1024 }'; }

if [[ -z "${BENCH_BIN:-}" ]]; then
  echo "== building (release)"
  cargo build --release -q
fi

if [[ ! -f "$SEED" ]]; then
  echo "== seeding $APPS apps x $DAYS days"
  python3 scripts/bench_seed.py --apps "$APPS" --days "$DAYS" --out "$SEED"
fi
rm -rf "$RUN"
mkdir -p "$RUN" "$DIR/results"
cp "$SEED" "$RUN/heartbeat.db"

hash="$(echo bench | "$BIN" hash-password 2>/dev/null)"
started="$(now_ms)"
env PORT="$PORT" AUTH_MODE=password ADMIN_EMAIL=bench@example.com ADMIN_PASSWORD_HASH="$hash" \
  ALLOWED_HOSTS=bench.invalid COOKIE_SECURE=false METRICS_TOKEN=bench \
  DATABASE_PATH="$RUN/heartbeat.db" APPS_FILE="$RUN/apps.json" UPTIME_DIR="$RUN/uptime" \
  SESSIONS_FILE="$RUN/sessions.json" NOTICES_FILE="$RUN/notices.json" \
  ALERT_TEMPLATES_FILE="$RUN/alert_templates.json" THEME_FILE="$RUN/theme.css" \
  "$BIN" >"$RUN/server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null' EXIT

for _ in $(seq 500); do
  curl -fsS "localhost:$PORT/healthz" >/dev/null 2>&1 && break
  kill -0 "$pid" 2>/dev/null || { echo "the server exited:"; cat "$RUN/server.log"; exit 1; }
  sleep 0.01
done
startup_ms=$(($(now_ms) - started))
sleep 1
idle_mb="$(rss_mb "$pid")"
echo "== up in ${startup_ms} ms, ${idle_mb} MB idle"

# Peak memory while k6 runs, sampled twice a second.
(while kill -0 "$pid" 2>/dev/null; do rss_mb "$pid"; echo; sleep 0.5; done) >"$RUN/rss.txt" &
sampler=$!

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
rev="$(git -C "$(dirname "$BIN")" rev-parse --short HEAD)$(git -C "$(dirname "$BIN")" diff --quiet || echo -dirty)"
k6_json="$RUN/k6.json"
echo "== load: ${VUS:-20} clients, ${DURATION:-15s} per endpoint"
k6 run --quiet -e BASE="http://localhost:$PORT" -e APPS="$APPS" \
  -e VUS="${VUS:-20}" -e DURATION="${DURATION:-15s}" -e OUT="$k6_json" bench/load.js || true
kill "$sampler" 2>/dev/null || true
wait "$sampler" 2>/dev/null || true
peak_mb="$(sort -n "$RUN/rss.txt" | tail -1)"
db_mb="$(du -m "$RUN/heartbeat.db" | cut -f1)"

result="$DIR/results/${stamp}-${rev}-${APPS}x${DAYS}.json"
python3 - "$k6_json" "$result" <<EOF
import json, platform, sys
k6 = json.load(open(sys.argv[1]))
json.dump({
    "at": "$stamp", "rev": "$rev", "machine": platform.platform(),
    "apps": $APPS, "days": $DAYS, "db_mb": $db_mb,
    "startup_ms": $startup_ms, "rss_idle_mb": $idle_mb, "rss_peak_mb": ${peak_mb:-0},
    **k6,
}, open(sys.argv[2], "w"), indent=2)
EOF
echo "startup ${startup_ms} ms · RSS ${idle_mb} MB idle, ${peak_mb} MB peak · DB ${db_mb} MB"
echo "saved $result"

// Load test for `just bench`: one scenario per endpoint, run one after another so each
// gets the whole machine. Needs the database from scripts/bench_seed.py (its session and
// embed token) and a server started by the recipe.
//
//   k6 run -e BASE=http://localhost:8199 -e APPS=100 bench/load.js
//
// Env: BASE, APPS (seeded app count), VUS (concurrent clients, 20), DURATION (per
// endpoint, 15s), OUT (summary JSON path).
import http from "k6/http";
import { check } from "k6";

const BASE = __ENV.BASE || "http://localhost:8199";
const APPS = Number(__ENV.APPS || 100);
const VUS = Number(__ENV.VUS || 20);
const DURATION = __ENV.DURATION || "15s";
const SESSION = "bench".repeat(12) + "0000";
const TOKEN = "bench-embed-token";

const app = () => `bench-${String(Math.floor(Math.random() * APPS)).padStart(4, "0")}`;
const session = { cookies: { heartbeat_session: SESSION } };

// name -> [path builder, request params]. Admin pages send the seeded session.
const ENDPOINTS = {
  healthz: [() => "/healthz", {}],
  dashboard_api: [() => "/api/uptime", session],
  detail_6h: [() => `/api/uptime/${app()}?hours=6`, session],
  detail_30d: [() => `/api/uptime/${app()}?hours=720`, session],
  status_page: [() => `/status/${app()}`, {}],
  embed: [() => `/embed/${app()}?token=${TOKEN}`, {}],
  badge: [() => `/badge/${app()}?token=${TOKEN}`, {}],
  metrics: [() => "/metrics", { headers: { Authorization: "Bearer bench" } }],
  audit_page: [() => "/audit", session],
};

const seconds = (d) => Number(d.replace(/s$/, ""));
export const options = {
  scenarios: Object.fromEntries(
    Object.keys(ENDPOINTS).map((name, i) => [
      name,
      {
        executor: "constant-vus",
        exec: "hit",
        vus: VUS,
        duration: DURATION,
        // 2 s gap so one endpoint's tail doesn't overlap the next.
        startTime: `${i * (seconds(DURATION) + 2)}s`,
        env: { ENDPOINT: name },
        tags: { endpoint: name },
      },
    ]),
  ),
  // One threshold per endpoint makes k6 keep its own latency and rate sub-metrics.
  thresholds: Object.fromEntries(
    Object.keys(ENDPOINTS).flatMap((name) => [
      [`http_req_duration{endpoint:${name}}`, ["p(99)<5000"]],
      [`http_req_failed{endpoint:${name}}`, ["rate<0.01"]],
      [`http_reqs{endpoint:${name}}`, ["count>0"]],
    ]),
  ),
  summaryTrendStats: ["avg", "med", "p(95)", "p(99)", "max"],
  // Only the status is checked: reading (and gunzipping) bodies would make k6, on the
  // same machine, the bottleneck instead of the server.
  discardResponseBodies: true,
};

export function hit() {
  const [path, params] = ENDPOINTS[__ENV.ENDPOINT];
  // What a browser sends: the server may answer compressed.
  const headers = { "Accept-Encoding": "gzip, deflate, br", ...(params.headers || {}) };
  const res = http.get(BASE + path(), { ...params, headers });
  check(res, { "status 200": (r) => r.status === 200 });
}

export function handleSummary(data) {
  const rows = Object.keys(ENDPOINTS).map((name) => {
    const d = data.metrics[`http_req_duration{endpoint:${name}}`]?.values ?? {};
    const reqs = data.metrics[`http_reqs{endpoint:${name}}`]?.values ?? {};
    const failed = data.metrics[`http_req_failed{endpoint:${name}}`]?.values ?? {};
    return {
      endpoint: name,
      rps: Math.round((reqs.count ?? 0) / seconds(DURATION)),
      p50_ms: d.med,
      p95_ms: d["p(95)"],
      p99_ms: d["p(99)"],
      max_ms: d.max,
      failed: failed.rate ?? 0,
    };
  });
  const ms = (v) => (v == null ? "-" : v.toFixed(1)).padStart(9);
  const table = [
    `${"endpoint".padEnd(15)}${"req/s".padStart(8)}${"p50 ms".padStart(9)}${"p95 ms".padStart(9)}${"p99 ms".padStart(9)}${"max ms".padStart(9)}  errors`,
    ...rows.map(
      (r) =>
        `${r.endpoint.padEnd(15)}${String(r.rps).padStart(8)}${ms(r.p50_ms)}${ms(r.p95_ms)}${ms(r.p99_ms)}${ms(r.max_ms)}  ${(r.failed * 100).toFixed(2)}%`,
    ),
  ].join("\n");
  const out = { stdout: `\n${table}\n` };
  if (__ENV.OUT) out[__ENV.OUT] = JSON.stringify({ vus: VUS, duration: DURATION, apps: APPS, endpoints: rows }, null, 2);
  return out;
}

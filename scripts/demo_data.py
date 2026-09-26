"""Demo data for `just demo`: apps.json + 30 days of uptime/*.jsonl, relative to *now*,
written to data/demo/. Every app lands in a different state (steady, flapping, down,
degrading, recovered, brand new, never monitored). The health URLs are chosen so live
checks keep each state: httpbin.org answers 200 (up) or after 2 s (degraded), and the
reserved .invalid TLD never resolves (down).

Embed test from a different origin, the way a third-party site would load it:
    python3 -m http.server 8100 -d data/demo  ->  http://localhost:8100/embed-test.html
"""
import base64, json, math, os, random, secrets, time
from pathlib import Path

# App names and the few check messages stored in the history follow APP_LANG, like the UI.
LANG = "en" if os.environ.get("APP_LANG", "es").strip().lower() == "en" else "es"
NAMES = {
    "es": {"estable": "Estable", "backend": "Backend", "intermitente": "Intermitente", "caida": "Caída",
           "degradada": "Degradada", "recuperada": "Recuperada", "nueva": "Recién registrada",
           "frontend": "Frontend", "frontend-roto": "Frontend roto", "sin-health": "Sin health"},
    "en": {"estable": "Steady", "backend": "Backend", "intermitente": "Flaky", "caida": "Down",
           "degradada": "Degraded", "recuperada": "Recovered", "nueva": "Just registered",
           "frontend": "Frontend", "frontend-roto": "Broken frontend", "sin-health": "No health"},
}[LANG]
BROKEN_BUNDLE = {
    "es": "Bundle roto: /html respondió text/html",
    "en": "Broken bundle: /html answered text/html",
}[LANG]

def app_name(key):
    return f"Demo · {NAMES[key]}"

random.seed(7)
HERE = Path(__file__).resolve().parent.parent / "data" / "demo"
UP_DIR = HERE / "uptime"
UP_DIR.mkdir(parents=True, exist_ok=True)
for f in UP_DIR.glob("*.jsonl"):
    f.unlink()

NOW = int(time.time())
STEP = 60
DAY = 86400
REAL_HEALTH = "https://httpbin.org/status/200"          # live checks keep these green
DEAD_HEALTH = "https://down.heartbeat.invalid/health"   # never resolves: live checks keep this red
SLOW_HEALTH = "https://httpbin.org/delay/2"             # 200 after ~2 s: live checks keep this yellow
REFUSED = "error sending request for url (https://down.heartbeat.invalid/health): client error (Connect): dns error: failed to lookup address information"
TIMEOUT = "error sending request: operation timed out"

DEGRADED_MS = 800  # keep in sync with UPTIME_DEGRADED_MS

def up(at, ms):
    status = "degraded" if ms > DEGRADED_MS else "up"
    return {"at": at, "status": status, "latency_ms": int(ms), "message": "HTTP 200 OK"}

def down(at, msg, ms=None):
    return {"at": at, "status": "down", "latency_ms": None if ms is None else int(ms), "message": msg}

def noise(base, jitter=0.25):
    ms = base * random.uniform(1 - jitter, 1 + jitter)
    if random.random() < 0.01:          # occasional spike
        ms *= random.uniform(2, 4)
    return ms

def series(start, fn):
    return [fn(at, NOW - at) for at in range(start, NOW - 5, STEP)]

# 1. Always up, fast.
def estable(at, ago):
    return up(at, noise(40 + 8 * math.sin(at / 3600)))

# 2. Mostly up; a 15-min outage 3 days ago and a 5-min 503 blip 9 days ago.
def backend(at, ago):
    if 3 * DAY <= ago < 3 * DAY + 900:
        return down(at, REFUSED)
    if 9 * DAY <= ago < 9 * DAY + 300:
        return down(at, "HTTP 503 Service Unavailable", noise(30))
    return up(at, noise(420, 0.15))

# 3. Flapping: random drops all month (worse in the last day) + a 40-min outage 2 h ago.
def intermitente(at, ago):
    if 2 * 3600 <= ago < 2 * 3600 + 2400:
        return down(at, "HTTP 502 Bad Gateway", noise(60))
    p = 0.08 if ago < DAY else 0.02
    if random.random() < p:
        return down(at, random.choice([TIMEOUT, "HTTP 502 Bad Gateway"]), None)
    return up(at, noise(180, 0.4))

# 4. Healthy until 45 min ago, down since (and stays down: its health URL is dead).
def caida(at, ago):
    if ago < 45 * 60:
        return down(at, REFUSED)
    return up(at, noise(90))

# 5. Degrading: latency climbs over the last 6 h, with timeouts/503s showing up at the end.
def degradada(at, ago):
    if ago > 6 * 3600:
        return up(at, noise(80))
    progress = 1 - ago / (6 * 3600)     # 0 -> 1 over the last 6 h
    ms = 80 + 1600 * progress ** 2
    if progress > 0.6 and random.random() < 0.15 * progress:
        return down(at, random.choice([TIMEOUT, "HTTP 503 Service Unavailable"]), None)
    return up(at, noise(ms, 0.2))

# 6. Recovered: down for 6 h yesterday, then back up.
def recuperada(at, ago):
    if 20 * 3600 <= ago < 26 * 3600:
        return down(at, "HTTP 500 Internal Server Error", noise(25))
    return up(at, noise(150))

apps = [
    ("demo-estable", app_name("estable"), REAL_HEALTH, NOW - 30 * DAY, estable),
    ("demo-backend", app_name("backend"), REAL_HEALTH, NOW - 30 * DAY, backend),
    ("demo-intermitente", app_name("intermitente"), REAL_HEALTH, NOW - 30 * DAY, intermitente),
    ("demo-caida", app_name("caida"), DEAD_HEALTH, NOW - 30 * DAY, caida),
    ("demo-degradada", app_name("degradada"), SLOW_HEALTH, NOW - 7 * DAY, degradada),
    ("demo-recuperada", app_name("recuperada"), REAL_HEALTH, NOW - 14 * DAY, recuperada),
    ("demo-nueva", app_name("nueva"), REAL_HEALTH, NOW - 12 * STEP, estable),
]

registry = []
for slug, name, health, start, fn in apps:
    beats = series(start, fn)
    (UP_DIR / f"{slug}.jsonl").write_text("".join(json.dumps(b) + "\n" for b in beats))
    registry.append({"slug": slug, "name": name, "logs_url": f"https://logs.heartbeat.invalid/{slug}/logs", "health_url": health, "embed_token": secrets.token_hex(24)})
    print(f"{slug:20} {len(beats):6} beats")

# 8. Single-page frontends: monitor-only (no logs URL) with the bundle check on. httpbin
#    serves the "index.html" (/base64/...) and its assets with whatever Content-Type the
#    URL asks for, so one page's bundles load fine and the other's JS comes back as HTML --
#    what an incomplete SPA deploy looks like.
def spa_page(script, stylesheet):
    html = (f'<!doctype html><html><head><title>Demo SPA</title>'
            f'<script type="module" src="{script}"></script>'
            f'<link rel="stylesheet" href="{stylesheet}"></head>'
            f'<body><div id="app"></div></body></html>')
    return "https://httpbin.org/base64/" + base64.urlsafe_b64encode(html.encode()).decode()

GOOD_JS = "https://httpbin.org/response-headers?Content-Type=text/javascript"
GOOD_CSS = "https://httpbin.org/response-headers?Content-Type=text/css"
BROKEN_JS = "https://httpbin.org/html"   # 200 text/html: the missing-bundle fallback

def frontend(at, ago):
    return up(at, noise(120))

def frontend_broken(at, ago):
    if ago < 20 * 60:
        return down(at, BROKEN_BUNDLE, noise(110))
    return up(at, noise(110))

for slug, name, health, fn in [
    ("demo-frontend", app_name("frontend"), spa_page(GOOD_JS, GOOD_CSS), frontend),
    ("demo-frontend-roto", app_name("frontend-roto"), spa_page(BROKEN_JS, GOOD_CSS), frontend_broken),
]:
    beats = series(NOW - 3 * DAY, fn)
    (UP_DIR / f"{slug}.jsonl").write_text("".join(json.dumps(b) + "\n" for b in beats))
    registry.append({"slug": slug, "name": name, "health_url": health, "check_assets": True,
                     "expect_body": "Demo SPA", "embed_token": secrets.token_hex(24)})
    print(f"{slug:20} {len(beats):6} beats")

# 7. Legacy entry registered before health URLs existed: never monitored.
registry.append({"slug": "demo-sin-health", "name": app_name("sin-health"), "logs_url": "https://logs.heartbeat.invalid/demo-sin-health/logs", "embed_token": secrets.token_hex(24)})

(HERE / "apps.json").write_text(json.dumps(registry, indent=2, ensure_ascii=False))
print(f"apps.json: {len(registry)} apps")

HEARTBEAT = "http://localhost:8090"
by_slug = {a["slug"]: a for a in registry}
widgets = "\n".join(
    f'    <heartbeat-status app="{a["slug"]}" token="{a["embed_token"]}"{theme}></heartbeat-status>'
    for a in registry
    for theme in ([""] if a["slug"] != "demo-degradada" else ["", ' theme="light"'])
)
(HERE / "embed-test.html").write_text(f"""<!doctype html>
<html lang="es">
<head><meta charset="utf-8"><title>Embed test</title>
<script src="{HEARTBEAT}/static/embed.js" defer></script>
<style>
  body {{ margin: 0; padding: 32px; background: #f4f5f7; font-family: Georgia, serif; }}
  .grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px; }}
  /* Hostile host styles: must not leak into the widget's Shadow DOM. */
  span {{ color: magenta !important; font-size: 30px; }}
  heartbeat-status.css-vars {{ --hb-bg: #1e1b2e; --hb-up: #a78bfa; --hb-degraded: #facc15; --hb-empty: #3b3552; }}
</style></head>
<body>
  <h1>Otra web</h1>
  <div class="grid">
{widgets}
    <heartbeat-status app="demo-estable" token="token-malo"></heartbeat-status>
    <heartbeat-status app="{by_slug['demo-intermitente']['slug']}" token="{by_slug['demo-intermitente']['embed_token']}"
      label="API de pagos" theme="light" color-up="#2563eb" color-degraded="#f97316" color-down="#be123c"
      color-bg="#eef2ff" color-border="#c7d2fe"></heartbeat-status>
    <heartbeat-status class="css-vars" app="{by_slug['demo-degradada']['slug']}" token="{by_slug['demo-degradada']['embed_token']}"
      label="Colores desde CSS"></heartbeat-status>
  </div>
</body>
</html>
""")
print("embed-test.html written")

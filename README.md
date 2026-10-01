# Heartbeat

**English** | [Español](README_ES.md)

Self-hosted uptime monitor and log viewer for your services. Register each app with a
health URL and a logs URL, and from a single place:

- see whether it's up, slow or down, with its history, latency and SSL certificate;
- get an alert on Slack, Discord or a webhook when it goes down or recovers;
- read its stdout and stderr live;
- publish its status on its own public page (`/status/{slug}`) or on any website with an embeddable
  component.

A single Rust binary with SQLite built in: everything is stored in one file,
`data/heartbeat.db`, with no database server to run.

## Screenshots

### Dashboard

<details open>
  <summary>Show screenshot</summary>
<img src=".github/public/dashboard_demo.png" alt="Dashboard: every app's status, the census strip, apps that need attention and recent events"/>
</details>

### App dashboard

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/app_dashboard_demo.png" alt="One app's detail: current latency, uptime, response-time chart, incidents with MTTR, and events"/>
</details>

### Logs

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/logs_demo.png" alt="Logs: list of apps with their check strip, to open their stdout and stderr"/>
</details>

### App logs

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/app_logs_demo.png" alt="Log viewer: stdout and stderr side by side, with level and module filters, search and live mode"/>
</details>

### Apps

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/apps_demo.png" alt="Apps: each app's state, pause menu, edit panel, and publishing with embed and badge"/>
</details>

### Notices

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/notice_demo.png" alt="Notices: publishing incident notices for one app's or every app's status page"/>
</details>

### Status app

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/status_app_demo.png" alt="An app's public status page: headline, open notices, 30 days of daily uptime and recent incidents"/>
</details>

### Settings

<details>
  <summary>Show screenshot</summary>
<img src=".github/public/settings_demo.png" alt="Settings: webhooks, alert message editor with a Slack-style preview, mentions and current configuration"/>
</details>

## Features

- **Traffic light per app**, every `UPTIME_INTERVAL_SECS` (60 by default) or on the app's
  own interval:
  - **Up (green)**: answers 2xx, or one of the app's expected status codes (e.g. `401`).
  - **Degraded (yellow)**: answers 2xx but slower than the threshold (the global
    `UPTIME_DEGRADED_MS`, or one set per app), or its certificate expires in fewer than
    `UPTIME_CERT_WARN_DAYS` days. It still counts as available for the uptime %.
  - **Down (red)**: any other status, takes longer than the timeout
    (`UPTIME_TIMEOUT_SECS`, 10 s, or one set per app), can't connect, or the response
    doesn't contain the configured keyword.

  Each app can send its own headers with the check (e.g. a token), and a
  `tcp://host:port` URL checks non-HTTP services (databases, queues) by opening a
  connection, under the same host policy.

  A failure is retried `UPTIME_RETRIES` times (5 s apart) before it's recorded, so a
  dropped packet doesn't paint the app red or send an alert.

- **Chat commands**: `/pulse status`, `pause` and `resume` from Slack and Discord; see
  [Chat commands](#chat-commands).
- **Alerts** (`ALERT_WEBHOOK_URLS`): only on confirmed status changes (down and recovery;
  degraded is optional with `ALERT_ON_DEGRADED`), naming the app and linking to it. Slack
  and Discord get their native format; any other URL gets a JSON event. `ALERT_MENTIONS`
  pings people when an app goes down: `here`, `channel`, Slack user (`U…`) or user group
  (`S…`) IDs, Discord user IDs or roles (`&…`); see `.env.example`.
  - Each app can add **its own webhooks and mentions** from `/apps`, on top of the global
    ones, and ping them from there.
  - A failed delivery is **retried** (2 s, 10 s, 30 s), and `/settings` shows each
    webhook's latest result.
  - While an app stays down, a **reminder** goes out every `ALERT_REMIND_MINS` (60 by
    default) saying how long it's been down.
  - **Mass outage**: when at least 3 apps and `UPTIME_MASS_DOWN_PCT` (50%) of the
    monitored ones go down at once, Heartbeat's own network is the likely culprit. One
    notice goes out and individual alerts wait; when it's over, only the apps still down
    alert.
- **Settings page** (`/settings`): ping each webhook and see its answer ("pong"),
  edit the text of each alert kind (down, still down, degraded, recovery) with a live
  preview and the variables `{app}`, `{message}`, `{latency}`, `{link}`, `{mentions}` and
  `{duration}`, send a test of
  each, and review the current configuration (secrets shown only as set / not set). Edited
  texts are stored in the database and apply without a restart; the defaults
  follow `APP_LANG`.
- **Frontends and single-page apps**: the logs URL is optional, so an app can be
  monitor-only. For SPAs (Vue, React…), "Verify JS/CSS bundles" reads the page's
  `<script>`/`<link rel="stylesheet">` references and checks each one really loads as
  JS/CSS. An SPA server answers 200 with the same `index.html` for every path -- often
  even for a missing bundle -- so an incomplete deploy that leaves users a blank page
  would otherwise look up.
- **Pause and maintenance**: a paused app isn't checked, and that time doesn't count
  against its uptime. A pause can last a set time (1 h to 7 days) and end by itself; an
  open-ended pause older than a day is flagged on the dashboard in case it was forgotten.
- **Editing**: everything can change without losing the history or the embed token.
- **Incidents**: each run of failed checks is an incident with its start, duration and
  cause; each app's detail lists the ones in the window, the MTTR and total downtime.
- **Public status page per app** (`/status/{slug}`): only for the apps you publish, with
  no URLs or error messages. It shows the current state, 90 days of daily uptime and the
  **notices** you publish from `/notices` (investigating, identified, monitoring,
  resolved) for that app; a notice naming no app shows on all of them. Resolved ones stay
  7 days as recent incidents. Any other slug answers the same 404.
- **Embed** for other websites, with one token per app, and an **SVG badge** for READMEs
  (see below).
- **Integrations**: `GET /metrics` in the Prometheus format (with `METRICS_TOKEN`) and
  each app's history exported to CSV or JSON from the dashboard.
- **Log viewer** with filters by level, module and text.
- **Audit log** (`/audit`): logins, every change, log views and chat commands, with who
  and when; see [Audit log](#audit-log).
- **Dead man's switch** (`HEARTBEAT_PING_URL`): Heartbeat pings that URL after every round
  of checks, so an external service warns you if Heartbeat itself stops. `GET /healthz`
  answers `ok` for load balancers.
- **Spanish or English UI** (`APP_LANG`).
- **System, dark, light or custom theme**, chosen per browser from the top bar; System
  (the default) follows the OS light/dark setting. The custom theme
  is CSS an admin saves in `/settings` (usually just overriding the color variables in
  `static/tokens.css`), stored in the database.

## Authentication

Two modes (`AUTH_MODE`):

- **`upstream`** (default): the login form forwards email and password to `LOGIN_URL`.
  The response must include `access_token` and `is_admin`; that decision belongs to the
  login server, Heartbeat doesn't reimplement it. The token is forwarded as a Bearer token
  to the logs endpoints.
- **`password`**: a local admin, no external server:
  ```bash
  echo 'your-password' | heartbeat hash-password   # prints the argon2 hash
  ```
  and in `.env`: `AUTH_MODE=password`, `ADMIN_EMAIL=...`, `ADMIN_PASSWORD_HASH=<hash>`.

**Read-only**: a viewer sees the dashboard and uptime data, but not logs, apps, notices
or settings, and can't change anything. In `password` mode it's `VIEWER_EMAIL` +
`VIEWER_PASSWORD_HASH`; in `upstream` mode, `UPSTREAM_VIEWERS=true` lets users without
`is_admin` in as viewers.

In both modes the cookie only holds a random session ID; the token stays on the server
(in the database, whose file is mode 600), so sessions survive a restart.

## Embed

Each app has an `embed_token`. `/apps` shows a preview and a "Copy code" button:

```html
<script src="https://HEARTBEAT_HOST/static/embed.js" defer></script>
<heartbeat-status app="SLUG" token="EMBED_TOKEN"></heartbeat-status>
```

Optional attributes:

- `label="My API"`: text shown instead of the app's name.
- `theme="light"`: dark by default.
- `lang="en"`: `es` or `en`; defaults to the server's `APP_LANG`.
- `bars="N"`: maximum number of bars; by default as many as fit the width, up to 100.
- `refresh="30"`: seconds between updates.
- Colors: `color-up`, `color-degraded`, `color-down`, `color-empty`, `color-bg`,
  `color-text`, `color-border`, with any CSS color. They can also come from the page's CSS
  (`heartbeat-status { --hb-up: #22c55e; }`); when both are set, the attribute wins.

The component uses Shadow DOM and only talks to `GET /embed/{slug}?token=...` (public,
with CORS), which never returns the health URL or error messages. The token is visible in
the page's HTML: if it leaks, "Rotate token" in `/apps` invalidates every earlier embed.

**Badge**, with the same token, for a README or a wiki:

```markdown
![Status](https://HEARTBEAT_HOST/badge/SLUG.svg?token=EMBED_TOKEN&label=API)
```

The state picks the text and color; the text follows `APP_LANG`:

| App state | Badge |
|---|---|
| Up | <img src=".github/public/badges/en-up.svg" alt="API: Operational"/> |
| Degraded (slow, or a certificate about to expire) | <img src=".github/public/badges/en-degraded.svg" alt="API: Slow"/> |
| Down | <img src=".github/public/badges/en-down.svg" alt="API: Down"/> |
| Paused, including scheduled maintenance | <img src=".github/public/badges/en-paused.svg" alt="API: Under maintenance"/> |
| Not checked yet, or no health URL | <img src=".github/public/badges/en-unknown.svg" alt="API: No data"/> |

Parameters:

- `token` (required): the app's embed token. A wrong one answers the same 404 as an
  unknown slug.
- `label` (optional): left-hand text instead of the app's name, up to 40 characters.

It's cached for 60 s and served with open CORS. "Rotate token" in `/apps` also
invalidates the badges already published.

## Metrics

With `METRICS_TOKEN` set, `GET /metrics` exposes each app's state in the Prometheus
format (`heartbeat_up`, `heartbeat_degraded`, `heartbeat_paused`,
`heartbeat_latency_milliseconds`, `heartbeat_uptime_24h_ratio`,
`heartbeat_uptime_30d_ratio`, `heartbeat_cert_expiry_timestamp_seconds`):

```yaml
scrape_configs:
  - job_name: heartbeat
    scheme: https
    authorization: { credentials: METRICS_TOKEN }
    static_configs: [{ targets: ["HEARTBEAT_HOST"] }]
```

## Chat commands

`/pulse` answers from Slack and Discord, in the channel it's typed in:

| Command | What it does | Who |
|---|---|---|
| `/pulse status` | Every app, worst first, with latency and 24 h uptime | Anyone |
| `/pulse status <app>` | One app: status, error, latency, uptime, pause, certificate | Anyone |
| `/pulse pause <app> [30m\|2h\|1d]` | Pauses its checks (no duration: until resumed) | `CHAT_ADMINS` |
| `/pulse resume <app>` | Resumes its checks | `CHAT_ADMINS` |
| `/pulse help` | Usage, only to you | Anyone |

`<app>` is the slug, the name, or part of either; Discord autocompletes it. Each
deployment creates its own Slack and Discord apps, so no request goes through a third
party. Both services must reach `PUBLIC_URL` over https.

**Slack**

1. `/settings` → *Chat commands* shows a manifest pointing at your `PUBLIC_URL`.
2. [api.slack.com/apps](https://api.slack.com/apps) → *Create New App* → *From a
   manifest* → paste it → *Install to Workspace*.
3. Copy *Basic Information* → *Signing Secret* to `SLACK_SIGNING_SECRET` and restart.

**Discord**

1. [discord.com/developers](https://discord.com/developers/applications) → *New
   Application*. Copy the *Application ID* and *Public Key* to `DISCORD_APPLICATION_ID`
   and `DISCORD_PUBLIC_KEY`, and *Bot* → *Reset Token* to `DISCORD_BOT_TOKEN`.
2. Restart Heartbeat: it registers the command on every start.
3. Paste `https://<PUBLIC_URL>/discord/interactions` as the *Interactions Endpoint URL*
   (Discord checks it right away, so Heartbeat must be running).
4. Open the invite link from `/settings` and pick the server.

`CHAT_ADMINS` lists who may pause and resume: Slack user IDs (`U…`), Discord user IDs and
Discord roles (`&…`), as in `ALERT_MENTIONS`. The command's name is `CHAT_COMMAND`.

## Audit log

`/audit` (admins only) lists who did what, newest first, and exports it as CSV:

- **Sessions**: logins (successful, failed, throttled) and logouts, with the email and IP.
- **Apps**: adding (its settings), editing (only the fields that changed, before and
  after), deleting, pausing and resuming, publishing, rotating the embed token, alert
  tests.
- **Chat**: every Slack and Discord command, including the ones refused for not being in
  `CHAT_ADMINS`, with the channel and what was typed.
- **Reading**: an app's logs (once per person and app every 15 minutes, since the viewer
  polls) and uptime exports.
- **Settings**: the theme (with its CSS), alert templates, alert tests and notices.
- **Heartbeat itself**: a scheduled pause ending.

It's stored in the `audit_log` table, filtered by who, action, app, source, outcome and
date, and kept `AUDIT_RETENTION_DAYS` (365; 0 = forever). Passwords and tokens are never
recorded, check headers appear by name only and webhook URLs are masked. Sessions started
before 0.3.1 have no email: their actions show as such until the next login.

## Endpoint contract

What your apps must expose to be registered.

**Health** (`GET`, no authentication): any 2xx counts as up. If you set a keyword, the
body must contain it (up to 256 KB is read).

**Logs** (`GET`, optional -- apps without one are monitor-only): Heartbeat sends:

- `stream=out|error|both` and `lines=N` (query).
- `Authorization: Bearer <admin's access_token>` (`upstream` mode).
- `X-Admin-Logs-Key: <ADMIN_LOGS_KEY>`, when configured.

And expects JSON shaped like this (lines oldest to newest, like `tail -n`):

```json
{
  "out":   { "lines": ["{\"timestamp\":\"...\",\"level\":\"INFO\",\"target\":\"...\",\"fields\":{...}}"], "error": null },
  "error": { "lines": ["raw stderr text"], "error": null }
}
```

- `out.lines`: one JSON line per event, in the
  [`tracing_subscriber::fmt::format::Json`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Json.html)
  format.
- `error.lines`: raw text.
- A per-stream `error` (string) means that stream couldn't be read.

The app authorizes the call with the user's JWT or, to read logs across environments,
with the shared key in the `X-Admin-Logs-Key` header (compared in constant time).

## Security

- **Server-side sessions** with 256-bit IDs; a forged cookie grants nothing and "Log out"
  (a POST) truly ends the session.
- **CSRF**: every POST requires an `Origin` (or `Referer`) from the same host, except
  `/slack/commands` and `/discord/interactions`, which read no session and only accept
  requests signed by Slack (HMAC-SHA256) or Discord (Ed25519) within the last 5 minutes.
- **Headers**: CSP (same-origin resources only), `frame-ancestors 'none'` and
  `X-Frame-Options: DENY` (no clickjacking), `nosniff`, `Referrer-Policy`.
- **Login throttling**: 5 failures within 15 minutes lock out that IP and that email.
  Behind a local proxy, the real IP comes from `X-Real-IP` only when the connection comes
  from loopback.
- **Outbound policy** (`src/outbound.rs`): the logs proxy and the checks only call
  `https://` (or `tcp://`) URLs on hosts in `ALLOWED_HOSTS`, never follow redirects, and
  refuse names that resolve to private, loopback or link-local IPs (including the cloud
  metadata address, `169.254.169.254`). Checked when saving and on every request. Each
  app's own webhooks must be `https://` and can't point at private IPs either.
- The logs proxy never echoes back the raw body of non-JSON responses.
- Fonts and every other asset are served by Heartbeat itself.

## Configuration

Every variable, with its default, is in [`.env.example`](.env.example). Only two are
required:

- `ALLOWED_HOSTS`: the hosts registered URLs may point at.
- `LOGIN_URL` in `upstream` mode, or `ADMIN_EMAIL` + `ADMIN_PASSWORD_HASH` in `password`
  mode.

Everything is stored in `DATABASE_PATH` (`./data/heartbeat.db`). Every check is kept
`UPTIME_RETENTION_DAYS` (30) for the charts, incidents and exports; the daily uptime,
`UPTIME_DAILY_RETENTION_DAYS` (400).

## Running locally

```bash
cp .env.example .env    # edit ALLOWED_HOSTS and authentication; COOKIE_SECURE=false without TLS
cargo run
```

Open `http://localhost:8090`.

With [`just`](https://github.com/casey/just):

- `just demo` (Spanish) or `just demo en` (English): starts with 90 days of sample data (one app in each state), imported
  with `heartbeat migrate` as if it came from 0.2, and a local admin; sign in
  at `http://localhost:8090` as `demo@example.com` / `demo` (admin) or
  `viewer@example.com` / `demo` (read-only). Each app's logs are generated live (the
  `demo` feature, never in a release) to match its state: the down one has errors and
  panics, the steady one mostly INFO. Requires Python 3.
- `just check`: formatting, `cargo deny` (vulnerabilities and licenses), pedantic clippy
  and tests, the same as CI.
- `just e2e`: browser smoke test against a throwaway server (requires Node).
- `just site`: builds the website (landing and these docs, from `site/`) and serves it on
  http://localhost:8200 (requires [uv](https://docs.astral.sh/uv/)). Pushes to `main` publish
  it to GitHub Pages.

## Benchmarks

`just bench [apps] [days]` (needs [k6](https://k6.io)) builds a release binary, seeds a
database with `apps` apps and `days` days of checks every minute (`scripts/bench_seed.py`,
cached per size in `data/bench/`), and loads each endpoint in turn: the dashboard API, an
app's detail (6 h and 30 days), the status page, embed, badge, `/metrics` and `/audit`.
It reports req/s, p50/p95/p99 per endpoint, startup time and memory (idle and peak RSS),
and saves the run to `data/bench/results/`.

```bash
just bench              # 100 apps, 30 days
just bench 500          # 500 apps
VUS=50 DURATION=30s just bench
just bench-compare      # latest run of each size, side by side
just bench-compare data/bench/results/a.json data/bench/results/b.json
```

The seeded apps have no health URL, so the monitor doesn't check them during the run: it
measures serving the stored history. Results depend on the machine; compare runs from the
same one.

### Results on a MacBook M1 (8 GB)

**In short: every endpoint answers under 10 ms at p99, except `/audit` (20-49 ms) and an
app's uncached 30-day detail with 500 apps (~400 ms).**

Setup:

- Apple M1 (8 cores), 8 GB of RAM, macOS 26.3; `just bench 100` and `just bench 500`
  with the defaults.
- The database is seeded before anything is measured: 30 days of checks every minute,
  4.3 million for 100 apps (167 MB) and 21.6 million for 500 (806 MB), plus 50 000 audit
  entries.
- Endpoints are loaded one after another, never together: each gets 20 concurrent
  clients for 15 s, then a 2 s pause before the next.
- k6 runs on the same machine, over localhost, and competes with the server for CPU. The
  figures show what the server costs, not what a deployment answers over a network,
  behind nginx and TLS.

| Endpoint | 100 apps: req/s | p50 / p99 ms | 500 apps: req/s | p50 / p99 ms |
|---|---:|---:|---:|---:|
| `/healthz` | 42 900 | 0.3 / 1.6 | 41 900 | 0.3 / 1.7 |
| Dashboard (`/api/uptime`) | 35 500 | 0.4 / 1.8 | 19 900 | 0.7 / 3.7 |
| App detail, 6 h | 39 800 | 0.3 / 1.6 | 6 400 | 2.9 / 9.7 |
| App detail, 30 days | 36 500 | 0.3 / 1.6 | 93 | 252 / 396 |
| Status page | 15 900 | 0.7 / 9.1 | 15 400 | 0.8 / 9.3 |
| Embed | 36 300 | 0.4 / 1.7 | 33 900 | 0.4 / 1.9 |
| Badge | 34 400 | 0.4 / 1.9 | 37 200 | 0.4 / 1.7 |
| `/metrics` | 29 300 | 0.4 / 2.5 | 22 300 | 0.6 / 3.4 |
| `/audit` (50 000 entries) | 2 600 | 5.2 / 49 | 3 700 | 4.7 / 20 |

| | 100 apps | 500 apps |
|---|---:|---:|
| Startup | 137 ms | 226 ms |
| Memory (RSS), idle / peak | 43 / 64 MB | 50 / 107 MB |

How to read it:

- An app's detail is cached until its next check, and the seeded apps aren't checked: with
  100 apps nearly every request after the first is a cache hit, while with 500 most are
  each app's first, so the 500-app 30-day figure is the uncached cost (~20 ms of CPU each,
  reading 43 000 checks). With `DATABASE_MMAP_MB=2048` it goes to 366 req/s (p50 66 ms),
  at the cost described in `.env.example`.
- Runs on a laptop vary about ±40 % between identical runs; repeat a run before reading a
  difference smaller than 2×.

## Deployment

### With Docker

```bash
cp .env.example .env   # edit
docker compose up -d
```

The image runs as an unprivileged user and stores everything in the `./data` volume.
Every release also publishes a multi-arch image (amd64 and arm64):

```bash
docker run -d --env-file .env -p 8090:8090 -v ./data:/app/data ghcr.io/OWNER/heartbeat:latest
```

### On a server (pm2 + nginx)

The binary runs under pm2, with nginx as a reverse proxy and a Let's Encrypt certificate.
The app listens on `8090`; that port is never exposed directly.

First time:

```bash
./prepare_ec2.sh /arena/heartbeat   # installs nginx/certbot/node/pm2/rust and builds
cp .env.example .env                # edit
pm2 start ./target/release/heartbeat --name heartbeat --cwd /arena/heartbeat
pm2 save && pm2 startup
```

nginx and certbot (see `deploy/nginx.conf.example`):

```bash
sudo cp deploy/nginx.conf.example /etc/nginx/sites-available/status.example.com
# edit REPLACE_ME_DOMAIN -> your domain
sudo ln -s /etc/nginx/sites-available/status.example.com /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d status.example.com
```

Updating without building on the server: every `v*` tag publishes Linux x86_64 and
aarch64 releases (`.github/workflows/release.yml`), and `deploy/update.sh` installs the
one for the server's architecture. Templates, static files and libs are inside the binary.
It never touches `.env`, verifies the checksum, backs up the database first (the latest 5
in `data/backups/`), keeps the previous binary and checks `/healthz`:

```bash
HEARTBEAT_REPO=owner/heartbeat deploy/update.sh          # latest release
HEARTBEAT_REPO=owner/heartbeat deploy/update.sh v1.2.0   # a specific one
deploy/update.sh --rollback                               # back to the previous binary
```

### Backups

`heartbeat backup <file>` writes a consistent copy of the database while Heartbeat keeps
running (SQLite's online backup), checks it, and never overwrites an existing file:

```bash
./target/release/heartbeat backup data/backups/heartbeat-$(date +%F).db
docker exec heartbeat heartbeat backup /app/data/backups/heartbeat.db   # with Docker
```

To restore one, stop Heartbeat, replace `data/heartbeat.db` with the copy, delete
`data/heartbeat.db-wal` and `data/heartbeat.db-shm`, and start it again.

### Upgrading from 0.2

0.2 kept its data in JSON files under `data/`; 0.3 keeps it in SQLite and won't start
until they're imported. `deploy/update.sh` does all of this by itself; by hand:

```bash
cp -r data data.bak                       # just in case
heartbeat migrate --dry-run               # what would be imported
heartbeat migrate                         # import, then rename the files to *.migrated
```

The import runs in a single transaction and checks the counts before committing: either
everything comes over or nothing changes. It brings apps, sessions (nobody has to sign in
again), notices, alert templates, the theme and the history: checks from the last 30
days, and older ones (up to 400 days) as daily uptime. The old files are only renamed
(`--keep` leaves them as they are), so going back to 0.2 is renaming them back;
`deploy/update.sh --rollback` does that too.

## Limits

The history lives in SQLite; only what the dashboard polls (latest checks, status
changes, 24 h and 30-day figures) is kept in memory. Measured with 100 apps checked every
minute for 30 days (4.3 million checks): a 110–190 MB database (depending on the check
messages), about 30 MB of RAM, startup in under half a second (about 1.3 s with a cold
disk cache), the dashboard API in 5 ms and an app's 30-day chart in 15 ms.

Every check runs from a single machine: mass-outage detection prevents an alert storm
when its network fails, but it's no substitute for probes from several regions.

## License

[Elastic License 2.0](LICENSE) (ELv2). You can use, copy, modify and redistribute
Heartbeat, including inside your company and for your own clients' services, but you
may not offer it to third parties as a hosted or managed service (a SaaS built on it).
It's source-available, not an OSI open-source license.

## Stack

- `actix-web` (HTTP server) and `tera` (server-side templates), with templates, static
  files and libs embedded in the binary (`rust-embed`; debug builds read them from disk).
- SQLite through `rusqlite`, compiled into the binary; the schema is in `migrations/`.
- Alpine.js vendored in `libs/alpinejs/`: interactivity without a build step.
- IBM Plex served locally (`static/fonts`, OFL license).
- UI strings in `locales/<lang>.json` (server) and `static/i18n/<lang>.js` (client).

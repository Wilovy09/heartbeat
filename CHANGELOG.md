# Changelog

All notable changes to Heartbeat. Versions follow [SemVer](https://semver.org/); entries
before 0.2.0 were reconstructed from the git history.

## [UNRELEASED - 0.3.2] - 2026-10-0?

* [ ] Benchmarks
```
y podemos hacer comparaciones con:

- Gatus: escrito en Go y muy ligero. Se configura todo en YAML, sin interfaz de edición, y permite condiciones avanzadas sobre la respuesta (status, body, tiempo, certificados). Es popular entre quienes prefieren "configuración como código".
- Checkmate (de Bluewave Labs): proyecto más reciente, con interfaz moderna. Además de uptime, incluye monitoreo de infraestructura (CPU, RAM, disco) mediante un agente.
- Kener: centrado en status pages bonitas, con monitoreo incluido. Está hecho en SvelteKit.
- Statping-ng: fork mantenido del antiguo Statping, en Go. Combina monitoreo y status page, aunque su desarrollo es menos activo.
- OneUptime: una plataforma mucho más grande y open source que junta uptime, incidentes, on-call, logs y APM. Es más pesada de desplegar.
- Uptime Kuma
```
* [ ] Email, telegram alerts
* [ ] Visor de memoria ram, cpu usage y disco del servidor de cada app

### Performance
- The dashboard's `/api/uptime` is built once per change (a round of checks, an app
  edited) instead of on every request, and served gzipped to clients that accept it,
  about 15 times smaller. With 500 apps and 30 days of checks: from 277 to ~19 600
  req/s, p50 from 70 to 0.7 ms, peak memory from 236 to 103 MB.
- `/metrics` uses the same cache: with 500 apps, from ~580 to ~12 000-19 000 req/s.
- Status changes are marked when a check is stored (`heartbeats.flip`, migration
  `0003_flips.sql`, which marks the existing history once) and read from a partial
  index: the recent events, rebuilt for every app at startup, no longer walk each
  app's history. With the 24 h figures counted by SQLite and apps rebuilt in parallel,
  startup with 500 apps goes from 4.3 s to ~0.2 s, with 100 from 0.7 s to ~0.1 s.
- An app's detail (the response-time chart and its incidents) is downsampled while
  reading, and the incidents read only the down checks and the flips: a 30-day window
  no longer loads its ~43 000 checks. It's cached per app and window until the app's
  next check, and gzipped.
- One reader connection per core (2 to 8, was 2).
- `DATABASE_MMAP_MB` (off by default) reads the database through a memory map: cold
  30-day charts under concurrency go from ~93 to ~370 req/s with 500 apps, but the
  process's RSS then counts the mapped file once per connection (see `.env.example`).
- Upgrading: the migration marks the existing history in one pass (well under a second
  for a few apps, ~35 s for 500 apps with 30 days); a 0.3.1 binary won't open the
  database afterwards, so rolling back means restoring the backup `deploy/update.sh`
  takes.

### Benchmarks
- `just bench [apps] [days]` (needs k6) seeds a synthetic database, loads every endpoint
  in turn and reports req/s, latency, startup time and memory; `just bench-compare` puts
  runs side by side, and `BENCH_BIN` runs another build on the same data.

## [0.3.1] - 2026-09-30

### Audit log
- `/audit` (admins only) lists logins, logouts, every change to apps, notices, alert
  templates and the theme, alert tests, uptime exports, log views (once per person and app
  every 15 minutes) and every chat command, allowed or refused, with who, when, from where
  and what changed. Filters by who, action, app, source, outcome and date; CSV export.
- Stored in the new `audit_log` table (migration `0002_audit.sql`) and kept
  `AUDIT_RETENTION_DAYS` (365; 0 = forever). No passwords or tokens; header values and
  webhook secrets are left out.
- Sessions now keep the email they logged in with. Sessions from 0.3.0 have none, so their
  actions are logged without it until the next login.

### Chat commands
- `/pulse status [app]`, `pause <app> [30m|2h|1d]`, `resume <app>` and `help` from Slack
  (`SLACK_SIGNING_SECRET`) and Discord (`DISCORD_APPLICATION_ID`, `DISCORD_PUBLIC_KEY`,
  `DISCORD_BOT_TOKEN`), answered over HTTP at `/slack/commands` and
  `/discord/interactions`. Each deployment uses its own Slack and Discord apps; `/settings`
  shows the Slack manifest, the Discord endpoint and the invite link.
- Every request must be signed (HMAC-SHA256 for Slack, Ed25519 for Discord) within the
  last 5 minutes. Pausing and resuming is limited to `CHAT_ADMINS` (Slack users, Discord
  users and roles); the command's name is `CHAT_COMMAND` (`pulse`).
- Discord registers the command on every start and autocompletes app names.

## [0.3.0] - 2026-09-29

### Upgrading from 0.2.x
- Everything moves to a SQLite database (`DATABASE_PATH`, `./data/heartbeat.db`), and
  the server won't start while 0.2's files sit next to an empty one. `deploy/update.sh`
  imports them by itself; by hand: back up `data/`, update, run `heartbeat migrate`
  (`--dry-run` first to see what it brings), then start. The old files are renamed to
  `*.migrated`, so going back is renaming them back (`deploy/update.sh --rollback` does
  it).
- `APPS_FILE`, `UPTIME_DIR`, `SESSIONS_FILE`, `NOTICES_FILE`, `ALERT_TEMPLATES_FILE` and
  `THEME_FILE` are now only read by `heartbeat migrate`.
- The 30-day uptime is counted over the last 30 calendar days (UTC, today included), from
  the daily counts, instead of the last 720 hours: figures can differ slightly.

### Storage
- SQLite, compiled into the binary (`rusqlite`), for apps, checks, sessions, notices,
  alert templates and the theme; the schema is in `migrations/` and applied on start. A
  database from a newer Heartbeat is refused instead of misread.
- Every check is kept `UPTIME_RETENTION_DAYS` (30); per-day counts, updated in the same
  transaction as each check, `UPTIME_DAILY_RETENTION_DAYS` (400). Old rows are deleted
  hourly; the JSONL files and their hourly rewrite are gone.
- Only what the dashboard polls stays in memory, rebuilt from the database on start. With
  100 apps and 30 days of checks (4.3 million): ~30 MB of RAM, start in under half a
  second, the dashboard API in ~5 ms.
- A round of checks is written in one transaction; if the write fails, the round still
  counts in memory (dashboard and alerts keep working) and the error is logged.
- `heartbeat migrate [--dry-run] [--keep]` imports a 0.2 install in one transaction,
  checking the counts before committing: live sessions, notices, templates, the theme,
  checks from the retention window and older ones (up to 400 days) as daily uptime. It
  streams the history instead of loading it, and never overwrites an earlier
  `*.migrated` copy.
- `heartbeat backup <file>`: a consistent copy of the running database (SQLite's online
  backup), checked before it's reported done, never overwriting a file.

### Status page
- 90 days of daily uptime instead of 30 (phones show the latest 30).

### Fixed
- Login (`upstream` mode): the log now says why the login server couldn't be reached
  (DNS, refused, TLS, timeout), and a login server that doesn't answer fails after 10 s
  instead of leaving the page loading.

### Deployment
- `deploy/update.sh` backs the database up before installing (the latest 5 in
  `data/backups/`); coming from 0.2 it archives the files there, stops the app and runs
  `heartbeat migrate`. `--rollback` to a 0.2 binary puts the files back.
- CI builds the Docker image and checks that it starts; the image now includes
  `migrations/`.

### Themes
- System (the default: follows the OS light/dark setting, live), dark, light and custom
  themes, picked per browser from the top bar (also on the public status pages; the login
  page follows the choice) and applied before the first paint.
- Custom theme: CSS saved from Settings and served at `/theme/custom.css`,
  applied on top of the dark tokens. The editor loads the current tokens as a template,
  loads or downloads a `.css` file and previews on the page before saving; `@import`,
  `url()` and anything over 32 KB are refused.
- Every color now comes from design tokens in `static/tokens.css`, shared by all pages;
  the latency chart reads them at draw time and redraws on a theme change, and the embed
  previews in Apps follow the page theme.

### Website
- Landing page and documentation in English and Spanish (`site/`), built from the READMEs
  with the app's own color tokens (system, light and dark) and published to GitHub Pages
  on every push to `main` (`.github/workflows/pages.yml`). `just site` previews it.
- Theme builder page: pick the base colors on a live copy of the dashboard, check the
  text contrast and copy or download the CSS for Settings → Custom theme.

## [0.2.0] - 2026-09-26

### Upgrading from 0.1.x
- Heartbeat is now licensed under the Elastic License 2.0 (`LICENSE`): free to use,
  modify and redistribute, but not to offer as a hosted or managed service.
- The public status page moved from `/status` to one page per app, `/status/{slug}`:
  update any link to the old page. Existing notices apply to every app until you edit
  them.
- Reminders are **on** by default (every 60 min while an app stays down): set
  `ALERT_REMIND_MINS=0` to keep the old behavior.
- Mass-outage detection is **on** by default (50%): set `UPTIME_MASS_DOWN_PCT=0` to
  turn it off.
- The server no longer speaks HTTP/2 or compresses responses; behind nginx (the
  documented setup) nothing changes.
- Startup and config error messages are now in English.
- New optional variables: `UPTIME_TIMEOUT_SECS`, `UPTIME_MASS_DOWN_PCT`,
  `ALERT_REMIND_MINS`, `VIEWER_EMAIL`, `VIEWER_PASSWORD_HASH`, `UPSTREAM_VIEWERS`,
  `NOTICES_FILE`, `METRICS_TOKEN` (see `.env.example`).

### Alerts
- Failed webhook deliveries are retried (2 s, 10 s, 30 s) on network errors, 5xx and
  429; `/settings` shows each webhook's last delivery.
- "Still down" reminders every `ALERT_REMIND_MINS` (default 60, 0 = off), with their own
  editable template and a `{duration}` variable (also available on recoveries).
- Mass-outage detection (`UPTIME_MASS_DOWN_PCT`, default 50%): when at least 3 apps and
  that share of the monitored ones are down at once, one notice goes to the global
  webhooks and individual down alerts are held until it ends.
- Per-app webhooks and mentions, added to the global ones, with a ping button in `/apps`.
  They must be https and can't resolve to private addresses.

### Checks
- Per-app check interval and timeout (`UPTIME_TIMEOUT_SECS` is the new global default).
- Expected status codes per app (e.g. accept `401`).
- Custom request headers per app.
- TCP checks with `tcp://host:port` health URLs, under the same allowlist and
  public-address policy.
- Scheduled maintenance: pause for a set time (1 h to 7 days from `/apps`, up to 30 days
  via the form) and resume automatically; open-ended pauses older than a day are
  flagged.

### Incidents and status page
- Incidents (runs of failed checks) with start, duration, cause, MTTR and total downtime
  in each app's detail.
- Incident notices managed from `/notices`, each for one app or all of them, and shown on
  those apps' status pages; resolved ones stay listed for 7 days.

### Log viewer
- Redesigned: one toolbar (stream, lines, search with `/`, live mode, refresh with `R`),
  aligned columns, local short times, each event's fields inline, errors and warnings
  marked, search highlighting, collapsible module filter, and empty states that say why.
- The app's health state is shown next to its name.
- stderr keeps terminal order (newest last), so multi-line panics read top to bottom.
- Lines are parsed once per load instead of on every render.

### Dashboard
- The status census is one proportional strip plus a legend; degraded and down counts
  are colored again (a CSS rule was overriding them).
- "Need attention" rows are aligned, say how long each app has been in that state, and
  name common network failures (DNS, timeout, refused, TLS) instead of the raw error; the
  full text is in the tooltip.
- Events show the latency for up/degraded changes instead of a repeated "HTTP 200 OK".
- The header adds the fleet's mean 24 h availability.

### Apps page
- Registering an app happens in a modal ("Register app" in the header), so the list gets
  the whole width; a rejected submission reopens it with the error and what was typed.
- Each app is a compact card: its live state and 24 h uptime in the header, a "Pause ▾"
  menu with the durations, and "Edit" / "Publish · embed and badge" collapsed on one row.
- "Delete" moved into the edit panel's danger zone and "Rotate token" into the publish
  panel, away from everyday actions; the publish panel also shows the badge, copies its
  Markdown and copies the app's status page link.
- A filter above the list (from 4 apps up).
- Text areas (headers, webhooks) use the same dark input style as the other fields.

### Settings page
- Alert templates are edited one kind at a time (tabs marking custom and unsaved ones),
  with the editor next to a preview rendered as the Slack message it becomes: mentions
  and links as the channel shows them, not their markup.
- Variables are listed once, each with what it holds; actions (restore, save, send test)
  sit in one aligned footer.
- The current configuration is grouped into access, checks and alerts.

### Public status page
- One page per published app, `/status/{slug}`, instead of a single page listing every
  public app; `/status` itself is gone. Unpublished or unknown slugs answer the same 404.
- 30 daily bars (green from 99.95%, amber from 99%, red below, gray without data) with
  the period's uptime; the tooltip gives the day and its uptime.
- The headline reflects open notices ("We're handling an incident") instead of saying
  "No data yet" or "All operational" under an open incident.
- Notices carry their state as a chip with a colored dot and relative times; resolved
  ones show when they happened and how long they lasted.
- Times use the visitor's language and timezone.

### Integrations and access
- SVG status badge: `/badge/{slug}.svg?token=…&label=…`, with the five states shown in
  the README.
- Prometheus metrics at `/metrics`, enabled by `METRICS_TOKEN`.
- CSV/JSON export of an app's history.
- Read-only viewer role: `VIEWER_EMAIL`/`VIEWER_PASSWORD_HASH` in password mode,
  `UPSTREAM_VIEWERS=true` in upstream mode.

### Operations
- Templates, static files and vendored libs are embedded in the binary.
- Releases for x86_64 and aarch64 Linux, and a multi-arch Docker image on GHCR;
  `deploy/update.sh` picks the right architecture.
- CI runs `cargo deny` (advisories, licenses, sources); Dependabot for cargo, npm,
  actions and Docker.
- actix-web is built without HTTP/2 and compression, dropping `h2` 0.3
  (RUSTSEC-2026-0258).
- Startup errors are reported as one clean log line and exit code 1 instead of a panic.
- The apps registry is written atomically (temp file + rename).
- Check messages ("keyword missing", "certificate expires in N days", "(3 attempts)",
  broken bundles) and the logs proxy's errors follow `APP_LANG`; they were always in
  Spanish.
- `just demo en` runs the demo in English, app names included.
- `just demo` generates live logs for every demo app (cargo feature `demo`, never in
  release builds), with a read-only viewer account and 2-minute reminders.

## [0.1.3] - 2026-09-25
### Added
- Monitor-only apps (the logs URL is optional) and SPA bundle checks.

## [0.1.2] - 2026-09-25
### Added
- Alert message templates, editable from `/settings`.

## [0.1.1] - 2026-09-25
### Added
- Alert mentions (`ALERT_MENTIONS`) and a settings page to test webhooks.

## [0.1.0] - 2026-09-24
### Added
- Uptime monitoring with retries, webhook alerts, SSL expiry and keyword checks.
- Log viewer, public status page, embeddable status component.
- Server-side sessions, CSRF checks, security headers, login throttling, outbound policy.
- Password auth mode, Spanish and English UI, Docker image and release workflow.

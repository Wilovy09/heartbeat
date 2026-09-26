# Changelog

All notable changes to Heartbeat. Versions follow [SemVer](https://semver.org/); entries
before 0.2.0 were reconstructed from the git history.

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

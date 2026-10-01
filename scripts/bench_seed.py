"""Synthetic database for `just bench`: N apps with D days of checks every INTERVAL
seconds (up, slow spells and outages), their daily uptime, an audit log and one admin
session, written straight into a fresh SQLite file with Heartbeat's own migrations.

The apps have no health URL, so the running monitor never checks them: the benchmark
measures what serving the stored history costs, not the network.

    python3 scripts/bench_seed.py --apps 100 --days 30 --out data/bench/heartbeat.db

Deterministic (fixed seed) apart from "now", so runs of the same size are comparable.
"""
import argparse
import json
import random
import sqlite3
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DAY = 86400
# What `just bench` sends as the session cookie and the embed/badge token.
SESSION_ID = "bench" * 12 + "0000"
EMBED_TOKEN = "bench-embed-token"
AUDIT_ACTIONS = ["logs.view", "app.update", "chat.status", "login.ok", "app.pause", "app.resume"]


def parse_args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--apps", type=int, default=100)
    p.add_argument("--days", type=int, default=30)
    p.add_argument("--interval", type=int, default=60, help="seconds between checks")
    p.add_argument("--audit", type=int, default=50_000, help="audit log entries")
    p.add_argument("--out", type=Path, default=ROOT / "data" / "bench" / "heartbeat.db")
    return p.parse_args()


def migrate(conn):
    migrations = sorted((ROOT / "migrations").glob("*.sql"))
    for sql in migrations:
        conn.executescript(sql.read_text())
    conn.execute(f"PRAGMA user_version = {len(migrations)}")


def beats(rng, slug, start, end, interval):
    """One app's checks: a steady latency with noise, slow spells and a few outages."""
    base = rng.uniform(60, 400)
    outages = []
    for _ in range(rng.randint(0, 6)):
        at = rng.randrange(start, end)
        outages.append((at, at + rng.randint(2, 90) * 60))
    slow = []
    for _ in range(rng.randint(0, 10)):
        at = rng.randrange(start, end)
        slow.append((at, at + rng.randint(5, 120) * 60))
    at = start
    while at < end:
        if any(a <= at < b for a, b in outages):
            yield (slug, at, 2, None, "timeout after 10 s")
        else:
            latency = int(rng.gauss(base, base * 0.15))
            if any(a <= at < b for a, b in slow):
                latency += rng.randint(900, 3000)
            latency = max(latency, 5)
            # What a real check stores: "HTTP 200", like the dashboard reads it.
            yield (slug, at, 1 if latency > 800 else 0, latency, "HTTP 200")
        at += interval


def main():
    args = parse_args()
    rng = random.Random(7)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    for suffix in ("", "-wal", "-shm"):
        Path(f"{args.out}{suffix}").unlink(missing_ok=True)

    started = time.monotonic()
    conn = sqlite3.connect(args.out)
    conn.execute("PRAGMA journal_mode = WAL")
    conn.execute("PRAGMA synchronous = OFF")
    migrate(conn)

    now = int(time.time()) // args.interval * args.interval
    start = now - args.days * DAY
    apps = [(f"bench-{i:04d}", f"Bench app {i:04d}") for i in range(args.apps)]
    conn.executemany(
        "INSERT INTO apps (slug, position, name, health_url, logs_url, embed_token, public) "
        "VALUES (?, ?, ?, NULL, NULL, ?, 1)",
        [(slug, i, name, EMBED_TOKEN) for i, (slug, name) in enumerate(apps)],
    )

    total = 0
    for slug, _ in apps:
        rows = list(beats(rng, slug, start, now, args.interval))
        # `flip`: the status changed from the check before (what store::insert keeps).
        flips = [1 if i == 0 or rows[i - 1][2] != row[2] else 0 for i, row in enumerate(rows)]
        conn.executemany(
            "INSERT INTO heartbeats (slug, at, status, latency_ms, message, flip) VALUES (?, ?, ?, ?, ?, ?)",
            [(*row, flip) for row, flip in zip(rows, flips)],
        )
        days = {}
        for _, at, status, _, _ in rows:
            counts = days.setdefault(at // DAY * DAY, [0, 0, 0])
            counts[status] += 1
        conn.executemany(
            "INSERT INTO daily_uptime (slug, day, up, degraded, down) VALUES (?, ?, ?, ?, ?)",
            [(slug, day, *counts) for day, counts in days.items()],
        )
        total += len(rows)

    conn.executemany(
        "INSERT INTO audit_log (at, source, actor, action, target, outcome, detail, ip) "
        "VALUES (?, 'web', ?, ?, ?, 'ok', ?, '127.0.0.1')",
        (
            (
                rng.randrange(start, now),
                f"user{rng.randrange(20)}@example.com",
                rng.choice(AUDIT_ACTIONS),
                rng.choice(apps)[0],
                json.dumps({"bench": True}),
            )
            for _ in range(args.audit)
        ),
    )
    conn.execute(
        "INSERT INTO sessions (id, upstream_token, role, expires_at, email) VALUES (?, '', 'admin', ?, ?)",
        (SESSION_ID, now + 7 * DAY, "bench@example.com"),
    )
    conn.commit()
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    conn.close()

    size_mb = args.out.stat().st_size / 1e6
    print(
        f"seeded {args.apps} apps x {args.days} d ({total:,} checks, {args.audit:,} audit entries) "
        f"in {time.monotonic() - started:.1f} s: {size_mb:.0f} MB"
    )


if __name__ == "__main__":
    main()

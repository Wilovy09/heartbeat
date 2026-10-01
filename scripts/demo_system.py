"""24 hours of CPU, memory and disk samples for `just demo`, written straight into the demo
database after `heartbeat migrate` (0.2 had no system samples to import). Each demo app
with a system URL gets a server that fits it, as src/demo.rs answers live: the down one
filling up, the slow one busy, the flaky one spiking. Heartbeat's own host gets a calm one.

    python3 scripts/demo_system.py data/demo/heartbeat.db
"""
import math
import random
import sqlite3
import sys
import time

GB = 1 << 30
STEP = 60
DAY = 86400
# (cpu %, memory %, disk %) per profile, as in src/demo.rs.
PROFILES = {
    "caida": (64, 96, 93),
    "intermitente": (35, 62, 58),
    "degradada": (88, 71, 66),
}


def profile(slug):
    for key, values in PROFILES.items():
        if key in slug:
            return key, values
    return "estable", (18, 46, 41)


def main(path):
    rng = random.Random(7)
    conn = sqlite3.connect(path)
    now = int(time.time()) // STEP * STEP
    sources = [s for (s,) in conn.execute("SELECT slug FROM apps WHERE settings LIKE '%system_url%'")]
    rows = []
    for source in sources + ["@heartbeat"]:
        kind, (cpu, mem, disk) = ("host", (9, 52, 34)) if source == "@heartbeat" else profile(source)
        mem_total, disk_total = (8 * GB, 228 * GB) if source == "@heartbeat" else (4 * GB, 50 * GB)
        for at in range(now - DAY, now, STEP):
            hours_ago = (now - at) / 3600
            # A daily rhythm, noise, and each profile's own story.
            c = cpu + 8 * math.sin(at / 5400) + rng.gauss(0, 3)
            m = mem + 2 * math.sin(at / 9000) + rng.gauss(0, 0.5)
            d = disk
            if kind == "intermitente" and (at // 360) % 6 < 2:
                c = 92 + rng.gauss(0, 2)
            if kind == "caida":
                # Filling up over the day until it ran out.
                m = min(97, 60 + (24 - hours_ago) * 1.5 + rng.gauss(0, 0.5))
                d = min(93, 80 + (24 - hours_ago) * 0.55)
            c = max(0.0, min(100.0, c))
            rows.append((source, at, c, int(mem_total * max(0, min(100, m)) / 100), mem_total,
                         int(disk_total * d / 100), disk_total, round(c / 50, 2)))
    conn.executemany(
        "INSERT OR IGNORE INTO system_samples (source, at, cpu_pct, memory_used, memory_total, "
        "disk_used, disk_total, load1) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        rows,
    )
    conn.commit()
    print(f"system samples: {len(rows)} for {len(sources)} apps and this host")


if __name__ == "__main__":
    main(sys.argv[1])

"""Compares `just bench` runs side by side (data/bench/results/*.json).

    python3 scripts/bench_compare.py                 # the latest run of each size
    python3 scripts/bench_compare.py a.json b.json   # these runs, in this order

Each column is a run; each row an endpoint's req/s and p99, then startup and memory. The
last column, with two or more runs of the same size, is the change from the first to the
last.
"""
import json
import sys
from pathlib import Path

RESULTS = Path(__file__).resolve().parent.parent / "data" / "bench" / "results"


def load(paths):
    if paths:
        return [json.loads(Path(p).read_text()) for p in paths]
    latest = {}
    for path in sorted(RESULTS.glob("*.json")):
        run = json.loads(path.read_text())
        latest[(run["apps"], run["days"])] = run
    return [latest[k] for k in sorted(latest)]


def change(first, last, higher_is_better):
    if not first or first is None or last is None:
        return ""
    pct = (last - first) / first * 100
    better = pct > 0 if higher_is_better else pct < 0
    return f"{pct:+.0f}%{' ✓' if better and abs(pct) >= 5 else ' ✗' if abs(pct) >= 5 else ''}"


def main():
    runs = load(sys.argv[1:])
    if not runs:
        sys.exit(f"no results in {RESULTS}: run `just bench` first")
    heads = [f"{r['rev']} {r['apps']}x{r['days']}" for r in runs]
    width = max(14, *(len(h) for h in heads)) + 2
    # A change only means something between runs of the same size.
    delta = len(runs) > 1 and len({(r["apps"], r["days"]) for r in runs}) == 1
    print("".ljust(24) + "".join(h.rjust(width) for h in heads) + ("change".rjust(10) if delta else ""))

    def row(label, values, higher_is_better, fmt):
        cells = "".join((fmt(v) if v is not None else "-").rjust(width) for v in values)
        tail = change(values[0], values[-1], higher_is_better).rjust(10) if delta else ""
        print(label.ljust(24) + cells + tail)

    endpoints = [e["endpoint"] for e in runs[-1]["endpoints"]]
    for name in endpoints:
        stats = [next((e for e in r["endpoints"] if e["endpoint"] == name), {}) for r in runs]
        row(f"{name} req/s", [s.get("rps") for s in stats], True, lambda v: f"{v:,}")
        row(f"{name} p99 ms", [s.get("p99_ms") for s in stats], False, lambda v: f"{v:.1f}")
    row("startup ms", [r["startup_ms"] for r in runs], False, str)
    row("RSS idle MB", [r["rss_idle_mb"] for r in runs], False, lambda v: f"{v:.1f}")
    row("RSS peak MB", [r["rss_peak_mb"] for r in runs], False, lambda v: f"{v:.1f}")
    row("DB MB", [r["db_mb"] for r in runs], False, str)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Compare a kept day of one-minute sessions (`tf reference minutes`) with Databento's consolidated daily bars (E19-S52).

    python3 -I scripts/check_minutes.py SESSIONS_FILE DAILY_BARS_CSV DAILY_SYMBOLOGY_JSON [MIN_SHARE]

The sessions come from the Nasdaq feed (XNAS.BASIC) and the daily bars from the consolidated one (EQUS.SUMMARY), so a name that
trades mostly elsewhere differs; the check is that the two agree where they should. For every symbol in both it compares the
regular session's close, high and low, and prints the share within 0.5% and within 2%, for the names with at least 100,000 shares
in the daily bar. Exits 1 if the share within 2% of any of the three is below MIN_SHARE percent (default 90).
"""
import csv
import json
import sys


def main():
    sessions_path, bars_path, sym_path = sys.argv[1:4]
    min_share = float(sys.argv[4]) if len(sys.argv) > 4 else 90.0
    result = json.load(open(sym_path))["result"]
    names = {int(spans[0]["s"]): sym for sym, spans in result.items()}
    daily = {}
    for r in csv.DictReader(open(bars_path)):
        sym = names.get(int(r["instrument_id"]))
        if sym:
            daily[sym] = (int(r["high"]), int(r["low"]), int(r["close"]), int(r["volume"]))
    lines = open(sessions_path).read().splitlines()
    kept = {}
    for line in lines[2:]:
        c = line.split(",")
        if c[1] == "1" and c[4]:
            kept[c[0]] = (int(c[2]), int(c[3]), int(c[4]))
    both = [s for s in kept if s in daily and daily[s][3] >= 100_000]
    print(f"{len(kept)} symbols with a session, {len(daily)} daily bars, {len(both)} compared (100,000 shares or more)")
    bad = False
    for i, label in enumerate(["high", "low", "close"]):
        diffs = sorted(abs(kept[s][i] - daily[s][i]) * 10_000 // max(daily[s][i], 1) for s in both)  # in basis points
        within = lambda bp: 100.0 * sum(1 for d in diffs if d <= bp) / max(len(diffs), 1)
        median = diffs[len(diffs) // 2] if diffs else 0
        print(f"{label:5}: within 0.5% {within(50):5.1f}%   within 2% {within(200):5.1f}%   median {median} bp   worst {diffs[-1] if diffs else 0} bp")
        if within(200) < min_share:
            bad = True
    sys.exit(1 if bad else 0)


main()

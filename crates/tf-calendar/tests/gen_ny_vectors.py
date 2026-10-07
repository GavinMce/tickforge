"""Writes ny_midnights.txt: for every date 2024-01-01..2028-12-31 the UTC second of New York
local midnight, from Python's zoneinfo (the system tz database), one `YYYY-MM-DD seconds` per line,
and the UTC seconds of each year's two daylight-saving changes. The Rust tests read the file and
compare it with the crate's own rule; run `python3 gen_ny_vectors.py > ny_midnights.txt` to rewrite."""
import datetime as dt
from zoneinfo import ZoneInfo

ny = ZoneInfo("America/New_York")
utc = dt.timezone.utc
d = dt.date(2024, 1, 1)
while d <= dt.date(2028, 12, 31):
    t = dt.datetime(d.year, d.month, d.day, tzinfo=ny)
    print(f"{d.isoformat()} {int(t.astimezone(utc).timestamp())}")
    d += dt.timedelta(days=1)
for y in range(2024, 2029):
    prev = None
    t = dt.datetime(y, 1, 1, tzinfo=utc)
    end = dt.datetime(y + 1, 1, 1, tzinfo=utc)
    while t < end:
        off = t.astimezone(ny).utcoffset()
        if prev is not None and off != prev:
            # step back to find the exact second of the change
            lo, hi = t - dt.timedelta(hours=1), t
            while (hi - lo).total_seconds() > 1:
                mid = lo + (hi - lo) / 2
                mid = mid.replace(microsecond=0)
                if mid.astimezone(ny).utcoffset() == prev:
                    lo = mid
                else:
                    hi = mid
            print(f"change {int(hi.timestamp())} {int(off.total_seconds())}")
        prev = off
        t += dt.timedelta(hours=1)

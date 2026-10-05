#!/usr/bin/env python3
"""Reference vectors for the indicator tests, computed with exact fractions.

Run `python3 gen_indicator_vectors.py` to print the Rust constants pasted into
`src/indicators.rs` (tests module). The series comes from a fixed LCG, and every
indicator uses the textbook definition: EMA alpha = 2 / (n + 1) seeded with the
simple average of the first n values (or the first value); Wilder smoothing
avg = (avg * (n - 1) + x) / n for RSI and ATR; RSI in permille; prices in cents
times 10^7 (raw price units). Values are rounded to the nearest integer.
"""
from fractions import Fraction as F

SC = 10_000_000


def series():
    x, p, out = 12345, 5000, []
    for _ in range(40):
        x = (x * 1103515245 + 12345) % (2**31)
        p += (x % 41) - 20
        out.append(p)
    return out, x


def bars(ser, x):
    hi, lo, cl = [], [], []
    for v in ser:
        x = (x * 1103515245 + 12345) % (2**31)
        up = (x % 15) + 1
        x = (x * 1103515245 + 12345) % (2**31)
        dn = (x % 15) + 1
        hi.append(v + up)
        lo.append(v - dn)
        cl.append(v)
    return hi, lo, cl


def ema(vals, n, seed):
    a = F(2, n + 1)
    if seed == "sma":
        s = sum(vals[:n]) / n
        out = [None] * (n - 1) + [s]
        for v in vals[n:]:
            s += a * (v - s)
            out.append(s)
    else:
        s = vals[0]
        out = [s]
        for v in vals[1:]:
            s += a * (v - s)
            out.append(s)
    return out


def rsi(vals, n):
    g = [max(vals[i] - vals[i - 1], 0) for i in range(1, len(vals))]
    l = [max(vals[i - 1] - vals[i], 0) for i in range(1, len(vals))]
    ag, al = sum(g[:n]) / n, sum(l[:n]) / n

    def r(ag, al):
        return F(500) if ag + al == 0 else 1000 * ag / (ag + al)

    out = [None] * n + [r(ag, al)]
    for i in range(n, len(g)):
        ag = (ag * (n - 1) + g[i]) / n
        al = (al * (n - 1) + l[i]) / n
        out.append(r(ag, al))
    return out


def atr(h, l, c, n):
    tr = [F(h[0] - l[0])] + [
        F(max(h[i] - l[i], abs(h[i] - c[i - 1]), abs(l[i] - c[i - 1])))
        for i in range(1, len(h))
    ]
    a = sum(tr[:n]) / n
    out = [None] * (n - 1) + [a]
    for t in tr[n:]:
        a = (a * (n - 1) + t) / n
        out.append(a)
    return out


def emit(name, vals):
    items = ["NONE" if v is None else str(int(round(v))) for v in vals]
    rows = ", ".join(items)
    print(f"const {name}: [i64; {len(vals)}] = [{rows}];")


ser, x = series()
hi, lo, cl = bars(ser, x)
px = [F(v * SC) for v in ser]
emit("SERIES_CENTS", ser)
emit("HIGH_CENTS", hi)
emit("LOW_CENTS", lo)
emit("CLOSE_CENTS", cl)
emit("EMA5_SMA_SEED_RAW", ema(px, 5, "sma"))
emit("EMA5_FIRST_SEED_RAW", ema(px, 5, "first"))
emit("RSI14_PERMILLE_X1", rsi(px, 14))
emit("ATR14_RAW", atr([F(v * SC) for v in hi], [F(v * SC) for v in lo], [F(v * SC) for v in cl], 14))

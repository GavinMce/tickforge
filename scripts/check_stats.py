#!/usr/bin/env python3
"""An independent check of crates/tf-stats (E19-S14).

Written separately from the Rust, from the formulas in the papers and with the standard library only: the normal
distribution comes from math.erfc (the Rust uses its own series), the quantile by bisection on it, the generator is
SplitMix64 written again here, and the bootstrap follows the same rule (circular blocks, a start by the high half of a
64-bit draw times the number of days). It prints the numbers the Rust tests (crates/tf-stats/src/tests.rs) pin, for the
data set in that file, so the two cannot drift apart unnoticed:

    python3 scripts/check_stats.py
"""
import math

MASK = (1 << 64) - 1

# One basis-point result per trade, by day. Day 3 has no trade: it is a day, and counts as one.
P_DAYS = [
    [12.0, -5.0, 8.0],
    [-20.0, 4.0],
    [7.0],
    [],
    [15.0, 9.0, -3.0, 6.0],
    [-11.0, -2.0],
    [5.0, 5.0, 10.0],
    [-8.0],
    [22.0, -14.0],
    [3.0, 1.0, 9.0],
]
# The refinement: the plain version's trades without the losers it would not have taken (same signals).
R_DAYS = [[12.0, 8.0], [4.0], [], [], [15.0, 9.0, 6.0], [], [10.0], [], [22.0], [9.0]]


class SplitMix64:
    def __init__(self, seed):
        self.state = seed & MASK

    def next(self):
        self.state = (self.state + 0x9E3779B97F4A7C15) & MASK
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        return z ^ (z >> 31)

    def below(self, n):
        return (self.next() * n) >> 64


def cdf(x):
    return 0.5 * math.erfc(-x / math.sqrt(2.0))


def inv_cdf(p):
    if p > 0.5:
        return -inv_cdf(1.0 - p)
    lo, hi = -40.0, 0.0
    for _ in range(200):
        mid = 0.5 * (lo + hi)
        if cdf(mid) < p:
            lo = mid
        else:
            hi = mid
    return 0.5 * (lo + hi)


def ratio(days, idx):
    s = sum(sum(days[i]) for i in idx)
    n = sum(len(days[i]) for i in idx)
    return s / n if n else None


def cluster_se(days):
    d = len(days)
    m = ratio(days, range(d))
    n = sum(len(x) for x in days)
    ss = sum((sum(x) - m * len(x)) ** 2 for x in days)
    return math.sqrt(d / (d - 1) * ss) / n


def quantile(sorted_vals, p):
    h = (len(sorted_vals) - 1) * p
    lo = int(math.floor(h))
    frac = h - lo
    if lo + 1 < len(sorted_vals):
        return sorted_vals[lo] + frac * (sorted_vals[lo + 1] - sorted_vals[lo])
    return sorted_vals[lo]


def boot(d, estimate, stat, replicates, block, seed):
    rng = SplitMix64(seed)
    stats = []
    for _ in range(replicates):
        idx = []
        while len(idx) < d:
            start = rng.below(d)
            for k in range(block):
                if len(idx) == d:
                    break
                idx.append((start + k) % d)
        v = stat(idx)
        if v is not None:
            stats.append(v)
    n = len(stats)
    mean = sum(stats) / n
    se = math.sqrt(sum((v - mean) ** 2 for v in stats) / (n - 1))
    stats.sort()
    return dict(estimate=estimate, se=se, t=estimate / se, lo=quantile(stats, 0.025),
                hi=quantile(stats, 0.975), replicates=n)


def moments(x):
    n = len(x)
    mean = sum(x) / n
    m2 = sum((v - mean) ** 2 for v in x) / n
    m3 = sum((v - mean) ** 3 for v in x) / n
    m4 = sum((v - mean) ** 4 for v in x) / n
    return dict(n=n, mean=mean, sd=math.sqrt(m2 * n / (n - 1)), skew=m3 / m2 ** 1.5, kurt=m4 / m2 ** 2)


EULER = 0.5772156649015329


def expected_max(trials, var):
    z1 = inv_cdf(1.0 - 1.0 / trials) if trials < 1e6 else -inv_cdf(1.0 / trials)
    z2 = inv_cdf(1.0 - 1.0 / (trials * math.e))
    return math.sqrt(var) * ((1 - EULER) * z1 + EULER * z2)


def psr(sr, bench, n, skew, kurt):
    return cdf((sr - bench) * math.sqrt(n - 1) / math.sqrt(1 - skew * sr + (kurt - 1) / 4 * sr * sr))


def show(name, value):
    print(f"{name} = {value!r}")


if __name__ == "__main__":
    d = len(P_DAYS)
    all_idx = range(d)
    show("plain.trades", sum(len(x) for x in P_DAYS))
    show("plain.mean", ratio(P_DAYS, all_idx))
    show("plain.cluster_se", cluster_se(P_DAYS))
    b = boot(d, ratio(P_DAYS, all_idx), lambda idx: ratio(P_DAYS, idx), 500, 2, 42)
    for k in ("estimate", "se", "t", "lo", "hi", "replicates"):
        show(f"plain.boot.{k}", b[k])
    flat = [v for day in P_DAYS for v in day]
    wins = [v for v in flat if v > 0]
    losses = [-v for v in flat if v < 0]
    show("plain.hit_rate", len(wins) / len(flat))
    show("plain.payoff", (sum(wins) / len(wins)) / (sum(losses) / len(losses)))
    total = peak = dd = 0.0
    for v in flat:
        total += v
        peak = max(peak, total)
        dd = max(dd, peak - total)
    show("plain.max_drawdown", dd)
    daily = [sum(x) for x in P_DAYS]
    m = moments(daily)
    for k in ("mean", "sd", "skew", "kurt"):
        show(f"daily.{k}", m[k])
    sr = m["mean"] / m["sd"]
    show("daily.sharpe", sr)
    show("expected_max.n7.var0.04", expected_max(7, 0.04))
    show("expected_max.n100.var0.01", expected_max(100, 0.01))
    bench = expected_max(7, 0.04)
    show("psr.bench0", psr(sr, 0.0, m["n"], m["skew"], m["kurt"]))
    show("dsr.n7.var0.04", psr(sr, bench, m["n"], m["skew"], m["kurt"]))

    common = [i for i in range(d) if P_DAYS[i] and R_DAYS[i]]
    est = ratio(R_DAYS, common) - ratio(P_DAYS, common)
    show("paired.common_days", len(common))
    show("paired.refined_mean_on_common", ratio(R_DAYS, common))
    show("paired.plain_mean_on_common", ratio(P_DAYS, common))
    pb = boot(len(common), est,
              lambda idx: ratio(R_DAYS, [common[i] for i in idx]) - ratio(P_DAYS, [common[i] for i in idx]),
              500, 2, 42)
    for k in ("estimate", "se", "t", "lo", "hi", "replicates"):
        show(f"paired.boot.{k}", pb[k])

    for x in (-5.0, -1.0, 0.0, 1.96, 3.0, 6.0):
        show(f"cdf({x})", cdf(x))
    for p in (1e-10, 0.001, 0.025, 0.5, 0.9, 0.975, 0.999):
        show(f"inv_cdf({p})", inv_cdf(p))
    rng = SplitMix64(1)
    show("splitmix64(1)[0..3]", [rng.next() for _ in range(3)])
    rng = SplitMix64(7)
    show("below(10) x5 seed 7", [rng.below(10) for _ in range(5)])

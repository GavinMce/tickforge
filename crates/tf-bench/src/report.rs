//! Benchmark results: one flat JSON object per (commit, scenario) per line, so a
//! file of results is a history that can be appended to, compared and diffed.
//!
//! The format is written and read by hand (flat string and integer fields, no
//! escapes) to keep the project free of a serialisation dependency. Unknown
//! fields are ignored on read, so rows can gain fields without breaking old files.

use std::fmt::Write as _;

/// One scenario's result at one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub commit: String,
    pub scenario: String,
    pub arch: String,
    pub os: String,
    /// `release` or `debug`.
    pub profile: String,
    pub cpus: u64,
    pub symbols: u64,
    pub secs: u64,
    pub seed: u64,
    pub events: u64,
    pub events_per_s: u64,
    /// Per-event time inside the sink, as measured (includes the timer's own cost).
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub p999_ns: u64,
    pub max_ns: u64,
    /// The median cost of the timer itself, measured with an empty sink.
    pub timer_p50_ns: u64,
}

impl Row {
    /// A latency with the timer's median cost taken off (never below zero).
    pub fn net(&self, ns: u64) -> u64 {
        ns.saturating_sub(self.timer_p50_ns)
    }

    /// Same machine class and workload, so a comparison means something.
    pub fn comparable(&self, other: &Row) -> bool {
        (
            &self.arch,
            &self.os,
            &self.profile,
            self.symbols,
            self.secs,
            self.seed,
        ) == (
            &other.arch,
            &other.os,
            &other.profile,
            other.symbols,
            other.secs,
            other.seed,
        )
    }

    pub fn to_json(&self) -> String {
        let s = |v: &str| v.replace(['"', ',', '\\', '\n'], "_");
        format!(
            "{{\"commit\":\"{}\",\"scenario\":\"{}\",\"arch\":\"{}\",\"os\":\"{}\",\"profile\":\"{}\",\
\"cpus\":{},\"symbols\":{},\"secs\":{},\"seed\":{},\"events\":{},\"events_per_s\":{},\
\"p50_ns\":{},\"p99_ns\":{},\"p999_ns\":{},\"max_ns\":{},\"timer_p50_ns\":{}}}",
            s(&self.commit),
            s(&self.scenario),
            s(&self.arch),
            s(&self.os),
            s(&self.profile),
            self.cpus,
            self.symbols,
            self.secs,
            self.seed,
            self.events,
            self.events_per_s,
            self.p50_ns,
            self.p99_ns,
            self.p999_ns,
            self.max_ns,
            self.timer_p50_ns
        )
    }

    pub fn from_json(line: &str) -> Result<Row, String> {
        let body = line
            .trim()
            .strip_prefix('{')
            .and_then(|l| l.strip_suffix('}'))
            .ok_or("not a JSON object")?;
        let mut fields = std::collections::BTreeMap::new();
        for part in body.split(',') {
            let (k, v) = part
                .split_once(':')
                .ok_or_else(|| format!("bad field {part:?}"))?;
            fields.insert(k.trim().trim_matches('"').to_owned(), v.trim().to_owned());
        }
        let text = |k: &str| -> Result<String, String> {
            let v = fields.get(k).ok_or_else(|| format!("missing {k}"))?;
            Ok(v.trim_matches('"').to_owned())
        };
        let num = |k: &str| -> Result<u64, String> {
            fields
                .get(k)
                .ok_or_else(|| format!("missing {k}"))?
                .parse()
                .map_err(|_| format!("{k} is not a number"))
        };
        Ok(Row {
            commit: text("commit")?,
            scenario: text("scenario")?,
            arch: text("arch")?,
            os: text("os")?,
            profile: text("profile")?,
            cpus: num("cpus")?,
            symbols: num("symbols")?,
            secs: num("secs")?,
            seed: num("seed")?,
            events: num("events")?,
            events_per_s: num("events_per_s")?,
            p50_ns: num("p50_ns")?,
            p99_ns: num("p99_ns")?,
            p999_ns: num("p999_ns")?,
            max_ns: num("max_ns")?,
            timer_p50_ns: num("timer_p50_ns")?,
        })
    }
}

pub fn to_jsonl(rows: &[Row]) -> String {
    rows.iter().map(|r| r.to_json() + "\n").collect()
}

pub fn from_jsonl(text: &str) -> Result<Vec<Row>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| Row::from_json(l).map_err(|e| format!("line {}: {e}", i + 1)))
        .collect()
}

/// Change from a baseline row to a new one, in permille of the baseline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delta {
    pub scenario: String,
    pub throughput_permille: i64,
    pub p99_permille: i64,
    /// The p99 change in nanoseconds (net of the timer). A percentage of a
    /// near-zero latency means nothing, so a flag also needs this to be real.
    pub p99_delta_ns: i64,
}

fn change(old: u64, new: u64) -> i64 {
    if old == 0 {
        return 0;
    }
    ((i128::from(new) - i128::from(old)) * 1000 / i128::from(old)) as i64
}

/// Deltas for every scenario in `new` that has a comparable row in `base`
/// (the last such row, so a whole history file works as a baseline).
pub fn compare(base: &[Row], new: &[Row]) -> Vec<Delta> {
    new.iter()
        .filter_map(|n| {
            let b = base
                .iter()
                .rev()
                .find(|b| b.scenario == n.scenario && b.comparable(n))?;
            let (old_p99, new_p99) = (b.net(b.p99_ns), n.net(n.p99_ns));
            Some(Delta {
                scenario: n.scenario.clone(),
                throughput_permille: change(b.events_per_s, n.events_per_s),
                p99_permille: change(old_p99, new_p99),
                p99_delta_ns: new_p99 as i64 - old_p99 as i64,
            })
        })
        .collect()
}

fn pct(permille: i64) -> String {
    let sign = if permille < 0 { '-' } else { '+' };
    let a = permille.unsigned_abs();
    format!("{sign}{}.{}%", a / 10, a % 10)
}

/// A p99 rise is only flagged if it is also at least this many nanoseconds.
pub const P99_MIN_DELTA_NS: i64 = 25;

/// How big a change has to be before the table flags it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// Flag a throughput drop of more than this many permille.
    pub drop_permille: i64,
    /// Flag a p99 rise of more than this many permille (and [`P99_MIN_DELTA_NS`]).
    pub rise_permille: i64,
}

impl Thresholds {
    /// For a quiet machine, where run-to-run noise is around 5%.
    pub const LOCAL: Thresholds = Thresholds {
        drop_permille: 100,
        rise_permille: 250,
    };
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds::LOCAL
    }
}

impl Delta {
    pub fn regressed(&self, t: &Thresholds) -> bool {
        self.throughput_permille < -t.drop_permille
            || (self.p99_permille > t.rise_permille && self.p99_delta_ns >= P99_MIN_DELTA_NS)
    }
}

/// A markdown table of `new`, with deltas against `base` when there is a
/// comparable one. Latencies are net of the timer's median cost.
pub fn markdown(new: &[Row], base: Option<&[Row]>, t: &Thresholds) -> String {
    let mut out = String::new();
    if let Some(r) = new.first() {
        let _ = writeln!(
            out,
            "**commit** `{}` | {} {} {} | {} cpus | {} symbols x {} s (seed {}) | {} events",
            r.commit, r.arch, r.os, r.profile, r.cpus, r.symbols, r.secs, r.seed, r.events
        );
        let _ = writeln!(
            out,
            "\nper-event latency is time inside the sink, net of a timer cost of {} ns\n",
            r.timer_p50_ns
        );
    }
    let deltas = base.map(|b| compare(b, new)).unwrap_or_default();
    let _ = writeln!(
        out,
        "| scenario | events/s | p50 ns | p99 ns | p99.9 ns | max ns | vs baseline |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---|");
    let mut flagged = false;
    for r in new {
        let vs = match deltas.iter().find(|d| d.scenario == r.scenario) {
            Some(d) => {
                let bad = d.regressed(t);
                flagged |= bad;
                format!(
                    "{} events/s, {} p99{}",
                    pct(d.throughput_permille),
                    pct(d.p99_permille),
                    if bad { " !!" } else { "" }
                )
            }
            None if base.is_some() => "no comparable baseline".to_owned(),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            r.scenario,
            r.events_per_s,
            r.net(r.p50_ns),
            r.net(r.p99_ns),
            r.net(r.p999_ns),
            r.net(r.max_ns),
            vs
        );
    }
    if flagged {
        let _ = writeln!(
            out,
            "\n`!!` marks a throughput drop over {}% or a p99 rise over {}% (and at least {} ns) \
against the baseline. It is a prompt to look, not a verdict.",
            t.drop_permille / 10,
            t.rise_permille / 10,
            P99_MIN_DELTA_NS
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(scenario: &str, eps: u64, p99: u64) -> Row {
        Row {
            commit: "abc123".into(),
            scenario: scenario.into(),
            arch: "x86_64".into(),
            os: "linux".into(),
            profile: "release".into(),
            cpus: 4,
            symbols: 5000,
            secs: 400,
            seed: 1,
            events: 2_000_000,
            events_per_s: eps,
            p50_ns: 40,
            p99_ns: p99,
            p999_ns: p99 * 3,
            max_ns: 90_000,
            timer_p50_ns: 20,
        }
    }

    #[test]
    fn rows_round_trip_through_json_lines_and_ignore_unknown_fields() {
        let rows = vec![row("a/b", 30_000_000, 100), row("c", 1, 2)];
        let text = to_jsonl(&rows);
        assert_eq!(text.lines().count(), 2);
        assert_eq!(from_jsonl(&text).unwrap(), rows);

        let extended = text.replace("\"cpus\":4,", "\"cpus\":4,\"future\":\"x\",");
        assert_eq!(from_jsonl(&extended).unwrap(), rows);
        assert_eq!(from_jsonl("\n\n").unwrap(), vec![]);
        assert!(
            from_jsonl("{\"commit\":\"x\"}")
                .unwrap_err()
                .contains("missing")
        );
        assert!(from_jsonl("nope").unwrap_err().contains("line 1"));
    }

    #[test]
    fn text_fields_cannot_break_the_format() {
        let mut r = row("a", 1, 1);
        r.commit = "ev\"il,commit".into();
        let back = Row::from_json(&r.to_json()).unwrap();
        assert_eq!(back.commit, "ev_il_commit");
    }

    #[test]
    fn deltas_compare_the_last_comparable_baseline_row() {
        let base = vec![row("s", 1000, 120), row("s", 2000, 220)]; // 2 commits of history
        let new = vec![row("s", 1800, 320)];
        let d = compare(&base, &new);
        // events/s 2000 -> 1800 = -10.0%; net p99 200 -> 300 = +50.0%
        assert_eq!(
            d,
            vec![Delta {
                scenario: "s".into(),
                throughput_permille: -100,
                p99_permille: 500,
                p99_delta_ns: 100
            }]
        );

        let mut other = row("s", 2000, 220);
        other.symbols = 10;
        assert!(
            compare(&[other], &new).is_empty(),
            "a different workload is not comparable"
        );
        assert!(compare(&base, &[row("other", 1, 1)]).is_empty());
    }

    #[test]
    fn the_table_flags_regressions_and_formats_percentages() {
        assert_eq!(
            (pct(34), pct(-5), pct(0), pct(1000)),
            (
                "+3.4%".into(),
                "-0.5%".into(),
                "+0.0%".into(),
                "+100.0%".into()
            )
        );
        let base = vec![row("s", 2000, 220)];
        let bad = markdown(&[row("s", 1000, 220)], Some(&base), &Thresholds::LOCAL);
        assert!(
            bad.contains("-50.0% events/s") && bad.contains("!!") && bad.contains("prompt to look"),
            "{bad}"
        );
        let fine = markdown(&[row("s", 2000, 220)], Some(&base), &Thresholds::LOCAL);
        assert!(
            fine.contains("+0.0% events/s") && !fine.contains("!!"),
            "{fine}"
        );
        let none = markdown(&[row("s", 2000, 220)], None, &Thresholds::LOCAL);
        assert!(
            !none.contains("vs baseline |  |") && none.contains("| s | 2000 | 20 | 200 |"),
            "{none}"
        );
        assert!(
            markdown(&[row("s", 1, 1)], Some(&[]), &Thresholds::LOCAL)
                .contains("no comparable baseline")
        );
    }

    #[test]
    fn thresholds_decide_what_is_flagged_and_tiny_latencies_never_are() {
        let wide = Thresholds {
            drop_permille: 600,
            rise_permille: 1500,
        };
        // events/s -30%: flagged on a quiet machine, not under CI-wide thresholds.
        let base = vec![row("s", 1000, 220)];
        let new = vec![row("s", 700, 220)];
        let d = &compare(&base, &new)[0];
        assert!(d.regressed(&Thresholds::LOCAL) && !d.regressed(&wide));
        assert!(markdown(&new, Some(&base), &Thresholds::LOCAL).contains("!!"));
        assert!(!markdown(&new, Some(&base), &wide).contains("!!"));

        // p99 1 ns -> 2 ns is +100% of nothing: timer noise, never a regression.
        let t = &compare(&[row("s", 1000, 21)], &[row("s", 1000, 22)])[0];
        assert_eq!((t.p99_permille, t.p99_delta_ns), (1000, 1));
        assert!(!t.regressed(&Thresholds::LOCAL));
        // A big change in a real latency is (net 100 ns -> 300 ns).
        let big = &compare(&[row("s", 1000, 120)], &[row("s", 1000, 320)])[0];
        assert!(big.regressed(&Thresholds::LOCAL));
        // A small absolute rise is not, even at a large percentage (net 40 -> 60 ns).
        let small = &compare(&[row("s", 1000, 60)], &[row("s", 1000, 80)])[0];
        assert!(small.p99_permille >= 500 && !small.regressed(&Thresholds::LOCAL));
    }
}

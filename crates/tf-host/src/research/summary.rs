//! What a results directory holds, in words (`tf research show`, E19-S31).

use std::fmt::Write as _;

use super::run::{ResearchError, Results};

/// The raw 1e-9 dollars as dollars and cents.
pub(crate) fn money(raw: i128) -> String {
    let cents = (raw.abs() + 5_000_000) / 10_000_000;
    format!(
        "{}${}.{:02}",
        if raw < 0 && cents > 0 { "-" } else { "" },
        cents / 100,
        cents % 100
    )
}

/// Hundredths of a basis point as basis points.
pub(crate) fn bp(x100: i128) -> String {
    let a = x100.abs();
    format!(
        "{}{}.{:02}",
        if x100 < 0 { "-" } else { "" },
        a / 100,
        a % 100
    )
}

/// What a variant's trades came to: their net in raw dollars, how many made money (a trade that broke even did not), and the
/// mean net in hundredths of a basis point (0 for none).
pub(crate) fn tally(trips: &[&super::trips::Trip]) -> (i128, usize, i128) {
    let net: i128 = trips.iter().map(|t| i128::from(t.net)).sum();
    let wins = trips.iter().filter(|t| t.net > 0).count();
    let mean = if trips.is_empty() {
        0
    } else {
        trips
            .iter()
            .map(|t| i128::from(t.net_bps_x100))
            .sum::<i128>()
            / trips.len() as i128
    };
    (net, wins, mean)
}

/// The configuration of a results directory and, for each definition, what its days came to. A directory without its
/// configuration, or one that does not read, is an error (see [`Results::open`]).
pub fn describe(r: &Results) -> Result<String, ResearchError> {
    let dates = r.dates()?;
    let lines = r.definition_lines()?;
    let trips = r.trips()?;
    let cost = r.cost();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} definitions over {} days ({} to {}); {} round trips",
        lines.len(),
        dates.len(),
        dates.first().map_or("-", String::as_str),
        dates.last().map_or("-", String::as_str),
        trips.len()
    );
    let _ = writeln!(
        s,
        "costs: {} ms to the broker, borrow {} bp a year, Section 31 rates through {}, Trading Activity Fee through {}",
        cost.latency_ns / 1_000_000,
        cost.borrow_bps_per_year,
        cost.sec_through,
        cost.taf_through
    );
    if let Some(b) = r.budgets()? {
        let _ = writeln!(
            s,
            "budgets: {} divided over {} strategies",
            money(b.balance as i128),
            b.ids.len()
        );
    } else {
        let _ = writeln!(s, "budgets: none (the strategies ran unbudgeted)");
    }
    let good = dates.iter().filter(|d| r.ledger(d).is_ok()).count();
    let _ = writeln!(
        s,
        "ledgers: {good} of {} days have theirs{}",
        dates.len(),
        if good < dates.len() {
            " (run the rest again to have them)"
        } else {
            ""
        }
    );
    for l in &lines {
        let mine: Vec<_> = trips
            .iter()
            .filter(|t| t.variant == l.fingerprint)
            .collect();
        let (net, wins, mean) = tally(&mine);
        let _ = writeln!(
            s,
            "\n{} {} (variant {:016x})\n  {}",
            l.id, l.name, l.fingerprint, l.params
        );
        if mine.is_empty() {
            let _ = writeln!(s, "  no trades");
            continue;
        }
        let _ = writeln!(
            s,
            "  {} trades, net {} after costs, mean {} bp a trade, {} of {} made money",
            mine.len(),
            money(net),
            bp(mean),
            wins,
            mine.len()
        );
    }
    Ok(s)
}

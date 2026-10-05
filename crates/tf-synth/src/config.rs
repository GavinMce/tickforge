use tf_core::{Event, Header, InstrumentId, NANOS_PER_SEC, Nanos, News, ProviderId, SymbolTable};

use crate::rng::SplitMix64;
use crate::scenario::{PullbackKind, Scenario};

/// 2026-01-05 14:30:00 UTC (09:30 ET), a regular-session open.
pub const DEFAULT_SESSION_START: Nanos = 1_767_623_400_000_000_000;

#[derive(Clone, Debug)]
pub struct SymbolSpec {
    pub symbol: String,
    pub base_px_cents: i64,
    /// Mean gap between trades in a phase whose rate multiplier is x1.
    pub base_interval_ns: Nanos,
    /// Emit a quote after every N trades.
    pub quote_every: u32,
    pub scenario: Scenario,
    /// Scripted articles about this symbol.
    pub news: Vec<NewsSpec>,
}

/// How an article reads; the generator picks a matching headline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sentiment {
    Positive,
    Neutral,
    Negative,
}

/// A scripted article, published `offset_ns` after the scenario's catalyst
/// (the moment its move starts). Negative leads the move, positive lags it.
/// Publication is clamped to the session start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewsSpec {
    pub offset_ns: i64,
    pub sentiment: Sentiment,
}

/// What a `News` event refers to. Events carry only `article_id`; the rest
/// lives here, as it would in a real news store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Article {
    pub article_id: u64,
    pub instrument: InstrumentId,
    pub published: Nanos,
    pub sentiment: Sentiment,
    pub headline: String,
}

/// Arrival delay of an article after publication: 50 ms to 1.5 s.
const NEWS_DELAY_MIN_NS: u64 = 50_000_000;
const NEWS_DELAY_JITTER_NS: u64 = 1_450_000_000;
const NEWS_STREAM_SALT: u64 = 0x4E45_5753;

const POSITIVE: [&str; 3] = [
    "surges on record order",
    "announces breakthrough partnership",
    "raises full-year guidance",
];
const NEUTRAL: [&str; 2] = [
    "to present at investor conference",
    "schedules earnings call",
];
const NEGATIVE: [&str; 3] = [
    "plunges after guidance cut",
    "faces regulator probe",
    "announces share offering",
];

pub(crate) struct ScheduledNews {
    pub(crate) event: Event,
    pub(crate) article: Article,
}

#[derive(Clone, Debug)]
pub struct SynthConfig {
    pub seed: u64,
    pub session_start: Nanos,
    pub duration: Nanos,
    pub symbols: Vec<SymbolSpec>,
}

impl SynthConfig {
    /// A reproducible universe of `n` symbols; roughly `runner_permille`/1000
    /// of them get a runner scenario (half healthy, half dangerous pullback),
    /// the rest random-walk quietly.
    pub fn universe(seed: u64, n: usize, duration: Nanos, runner_permille: u32) -> Self {
        let mut rng = SplitMix64::fork(seed, 0x00C0_FFEE);
        let symbols = (0..n)
            .map(|i| {
                let base_px_cents = rng.range(150, 2_000) as i64;
                let base_interval_ns = rng.range(200, 5_000) * 1_000_000;
                let scenario = if rng.permille(runner_permille) {
                    let kind = if rng.permille(500) {
                        PullbackKind::Healthy
                    } else {
                        PullbackKind::Dangerous
                    };
                    Scenario::runner(kind, rng.range(30, 300) * NANOS_PER_SEC)
                } else {
                    Scenario::quiet()
                };
                SymbolSpec {
                    symbol: format!("SYN{i:04}"),
                    base_px_cents,
                    base_interval_ns,
                    quote_every: 2,
                    scenario,
                    news: Vec::new(),
                }
            })
            .collect();
        SynthConfig {
            seed,
            session_start: DEFAULT_SESSION_START,
            duration,
            symbols,
        }
    }

    /// Every scripted article with its `News` event, in arrival order. A pure
    /// function of the config: ids, delays and headlines come from RNG streams
    /// forked per (seed, symbol, article), so they do not depend on anything else.
    pub(crate) fn news_schedule(&self) -> Vec<ScheduledNews> {
        let end = self.session_start.saturating_add(self.duration);
        let mut out = Vec::new();
        for (i, spec) in self.symbols.iter().enumerate() {
            let instrument = InstrumentId::try_from(i).expect("more than u32::MAX symbols");
            for (k, n) in spec.news.iter().enumerate() {
                let mut rng =
                    SplitMix64::fork(self.seed ^ NEWS_STREAM_SALT, ((i as u64) << 20) | k as u64);
                let article_id = rng.next_u64();
                let delay = NEWS_DELAY_MIN_NS + rng.below(NEWS_DELAY_JITTER_NS);
                let phrases: &[&str] = match n.sentiment {
                    Sentiment::Positive => &POSITIVE,
                    Sentiment::Neutral => &NEUTRAL,
                    Sentiment::Negative => &NEGATIVE,
                };
                let phrase = phrases[rng.below(phrases.len() as u64) as usize];

                let published = self
                    .session_start
                    .saturating_add(spec.scenario.catalyst)
                    .saturating_add_signed(n.offset_ns)
                    .max(self.session_start);
                let ts_recv = published.saturating_add(delay);
                if ts_recv >= end {
                    continue;
                }
                out.push(ScheduledNews {
                    event: Event::News(News {
                        hdr: Header {
                            ts_event: published,
                            ts_recv,
                            seq: 0,
                            instrument,
                            provider: ProviderId::Synthetic,
                        },
                        article_id,
                    }),
                    article: Article {
                        article_id,
                        instrument,
                        published,
                        sentiment: n.sentiment,
                        headline: format!("{} {phrase}", spec.symbol),
                    },
                });
            }
        }
        out.sort_by_key(|s| (s.event.ts_recv(), s.event.instrument()));
        out
    }

    /// The articles behind this config's `News` events, in arrival order.
    pub fn articles(&self) -> Vec<Article> {
        self.news_schedule()
            .into_iter()
            .map(|s| s.article)
            .collect()
    }

    /// Symbol table whose ids line up with generator instrument ids.
    pub fn symbol_table(&self) -> SymbolTable {
        let mut t = SymbolTable::new();
        for s in &self.symbols {
            t.intern(&s.symbol);
        }
        t
    }
}

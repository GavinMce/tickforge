//! Why a strategy acted (E19-S32): a trace is a small table a strategy records when it decides, and the viewer only reads.
//!
//! A cross strategy that is asked to trace ([`crate::CrossStrategy::set_tracing`]) hands back [`Trace`]s beside its intents:
//! the evidence it had at the moment of a decision, in the words of the strategy (the whole ranked cross-section of the
//! closing reversal with the names chosen and the reason for every other, the draw of the null strategy). Recording changes
//! no decision (ADR 0025, ADR 0063): a test runs a day traced and untraced and compares the logs.
//!
//! A trace is a time, a kind, some named values (`head`) and a table (`columns` and `rows` of text). Everything is text so
//! that it reads back exactly and the viewer needs no knowledge of the strategy; a column called `instrument` holds
//! instrument numbers, which the host turns into symbols ([`Trace::resolve_symbols`]) so that a stored trace names no number
//! that means nothing without the day's symbol table.
//!
//! The text of a set of traces, as a file keeps it:
//!
//! ```text
//! traces v1
//! trace<TAB><strategy><TAB><time, ns><TAB><kind>
//! head<TAB><key><TAB><value>
//! cols<TAB><name><TAB><name>...
//! row<TAB><cell><TAB><cell>...
//! end <checksum of everything above, 16 hex digits>
//! ```
//!
//! Tabs, line breaks and backslashes in a value are written `\t`, `\n`, `\r` and `\\`.

use tf_core::Nanos;

const HEADER: &str = "traces v1";

/// A trace: what a strategy saw and chose at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    /// Event time of the decision.
    pub ts: Nanos,
    /// What it is a trace of (`rank`, `draw`, `entry`).
    pub kind: String,
    /// Named values: the settings in force and the totals.
    pub head: Vec<(String, String)>,
    pub columns: Vec<String>,
    /// Every row has as many cells as there are columns.
    pub rows: Vec<Vec<String>>,
}

impl Trace {
    pub fn new(ts: Nanos, kind: &str) -> Trace {
        Trace {
            ts,
            kind: kind.to_owned(),
            head: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Add a named value.
    pub fn with(mut self, key: &str, value: impl ToString) -> Trace {
        self.head.push((key.to_owned(), value.to_string()));
        self
    }

    /// Name the columns of the table.
    pub fn with_columns(mut self, columns: &[&str]) -> Trace {
        self.columns = columns.iter().map(|c| (*c).to_owned()).collect();
        self
    }

    /// Add a row; it must have a cell for each column.
    pub fn push_row(&mut self, cells: Vec<String>) {
        debug_assert_eq!(
            cells.len(),
            self.columns.len(),
            "a row has a cell for each column"
        );
        self.rows.push(cells);
    }

    /// The value of a named head entry.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.head
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Turn the instrument numbers of the `instrument` column into symbols, naming the column `symbol`; `name` gives the
    /// symbol of a number. A cell that is not a number is left as it is.
    pub fn resolve_symbols(&mut self, name: impl Fn(u32) -> String) {
        let Some(c) = self.columns.iter().position(|c| c == "instrument") else {
            return;
        };
        "symbol".clone_into(&mut self.columns[c]);
        for row in &mut self.rows {
            if let Ok(id) = row[c].parse::<u32>() {
                row[c] = name(id);
            }
        }
    }

    /// The first column's value on each row, for a column by name; `None` for no such column.
    pub fn column(&self, name: &str) -> Option<Vec<&str>> {
        let c = self.columns.iter().position(|x| x == name)?;
        Some(self.rows.iter().map(|r| r[c].as_str()).collect())
    }
}

/// Why the text of traces was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceError(pub String);

impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "traces: {}", self.0)
    }
}

impl std::error::Error for TraceError {}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unescape(s: &str) -> Result<String, TraceError> {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => o.push('\\'),
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            other => {
                return Err(TraceError(format!(
                    "an escape that is not one of ours: `\\{}`",
                    other.map_or(String::new(), String::from)
                )));
            }
        }
    }
    Ok(o)
}

/// The text of the traces of a day, each with the number of the strategy it is from, in the order given.
pub fn render_all(traces: &[(u16, Trace)]) -> String {
    let mut s = format!("{HEADER}\n");
    for (strategy, t) in traces {
        s.push_str(&format!(
            "trace\t{strategy}\t{}\t{}\n",
            t.ts,
            escape(&t.kind)
        ));
        for (k, v) in &t.head {
            s.push_str(&format!("head\t{}\t{}\n", escape(k), escape(v)));
        }
        if !t.columns.is_empty() {
            let cols: Vec<String> = t.columns.iter().map(|c| escape(c)).collect();
            s.push_str(&format!("cols\t{}\n", cols.join("\t")));
        }
        for r in &t.rows {
            let cells: Vec<String> = r.iter().map(|c| escape(c)).collect();
            s.push_str(&format!("row\t{}\n", cells.join("\t")));
        }
    }
    format!("{s}end {:016x}\n", fnv(s.as_bytes()))
}

/// Traces from their text. The text must be whole, unaltered and of this version: anything else is an error.
pub fn parse_all(text: &str) -> Result<Vec<(u16, Trace)>, TraceError> {
    let bad = |m: String| TraceError(m);
    let end = text
        .rfind("end ")
        .filter(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
        .ok_or_else(|| bad("cut short: no `end`".into()))?;
    let (body, tail) = text.split_at(end);
    if tail.matches('\n').count() != 1 || !tail.ends_with('\n') {
        return Err(bad("text after `end`".into()));
    }
    let want = u64::from_str_radix(tail[4..tail.len() - 1].trim(), 16)
        .map_err(|_| bad("the checksum is not hexadecimal".into()))?;
    if fnv(body.as_bytes()) != want {
        return Err(bad("the checksum does not match: edited or damaged".into()));
    }
    let mut lines = body.lines();
    if lines.next() != Some(HEADER) {
        return Err(bad(format!("not `{HEADER}`")));
    }
    let mut out: Vec<(u16, Trace)> = Vec::new();
    for line in lines {
        let w: Vec<&str> = line.split('\t').collect();
        match w.as_slice() {
            ["trace", strategy, ts, kind] => {
                let strategy = strategy
                    .parse()
                    .map_err(|_| bad(format!("`{strategy}` is not a strategy number")))?;
                let ts = ts
                    .parse()
                    .map_err(|_| bad(format!("`{ts}` is not a time")))?;
                out.push((strategy, Trace::new(ts, &unescape(kind)?)));
            }
            ["head", k, v] => {
                let t = &mut out
                    .last_mut()
                    .ok_or_else(|| bad("a `head` before any `trace`".into()))?
                    .1;
                t.head.push((unescape(k)?, unescape(v)?));
            }
            ["cols", cols @ ..] => {
                let t = &mut out
                    .last_mut()
                    .ok_or_else(|| bad("`cols` before any `trace`".into()))?
                    .1;
                if !t.columns.is_empty() || !t.rows.is_empty() {
                    return Err(bad("`cols` twice, or after a row".into()));
                }
                t.columns = cols.iter().map(|c| unescape(c)).collect::<Result<_, _>>()?;
            }
            ["row", cells @ ..] => {
                let t = &mut out
                    .last_mut()
                    .ok_or_else(|| bad("a `row` before any `trace`".into()))?
                    .1;
                if cells.len() != t.columns.len() {
                    return Err(bad(format!(
                        "a row of {} cells under {} columns",
                        cells.len(),
                        t.columns.len()
                    )));
                }
                t.rows.push(
                    cells
                        .iter()
                        .map(|c| unescape(c))
                        .collect::<Result<_, _>>()?,
                );
            }
            _ => return Err(bad(format!("a line this version does not know: `{line}`"))),
        }
    }
    Ok(out)
}

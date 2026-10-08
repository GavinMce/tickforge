# 0063. A cross strategy records why it acted as a table the host collects beside its decision log

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-186

## Context

ADR 0025 settled that the evidence for a decision is the strategy's own record, made at the moment it decided, and that the
viewer only reads. That was built for `MomentumLong` on one symbol. The strategies of the library are cross-sectional
(`CrossStrategy`): T04 ranks the whole universe once a day and the only trace of why it bought what it did was the buys. To look
at a trade in the backtest view (E19-S35) and judge it, the view needs the cross-section the strategy saw: which names were
ranked where, on which prices, and why every other name was not bought.

## Decision

- **A trace is a small table.** `tf_strategy::Trace`: a time, a kind (`rank`, `draw`, `entry`), named values (`head`) and rows of
  text under named columns. Text throughout, so it reads back exactly and the viewer needs to know nothing of the strategy. A
  set of traces is deterministic text with a checksum (`traces v1`): a damaged, cut or altered file is refused, as for the cost
  model and the trial registry.
- **The hook is two default methods of `CrossStrategy`:** `set_tracing(bool)` and `take_traces()`. A strategy that does not
  trace does nothing; one that does records only when asked, so an untraced run pays nothing. The type-erased runner passes them
  through, and **`Host::with_traces()`** asks every strategy (now and added later) and collects what they record beside their
  intents, by strategy number. Traces are **not** in the decision log: the replay check compares the log, and a trace that
  differed between a run and its replay would otherwise make an honest replay fail for something that decided nothing.
- **Recording changes no decision, and that is tested.** The decision and its trace come from one function: T04's `assess`
  says whether a name may be bought and, if not, why, and `picks` and the trace both call it, so the trace cannot say something the
  decision did not do. Tests run a day traced and untraced and compare the intents, the exits, the counts, and (through the
  host) the decision log and the fills.
- **Instrument numbers become symbols at the host.** A column named `instrument` holds a number; the host, which has the day's
  symbol table, renames it `symbol` and fills in the names (`Trace::resolve_symbols`), so a stored trace needs no table to read.
- **T04's trace is the whole cross-section at its one decision a day** (`rank`): every member, those that may be bought first
  (ranked by return, most negative first, ties to the lower number) and the rest by number; for each the prior close, the price at
  15:00 and where it came from (`tier0`, `snapshot`, `none`), the return in parts per million, the bid and ask at the decision,
  and a status: `entered`, `skipped:no_share` or `skipped:no_stop` for a name chosen that the money or the price did not allow,
  `not_chosen` for one that could have been bought and ranked below the first `names`, and `skipped:` with `halted`,
  `restricted`, `no_prior`, `no_price`, `no_quote`, `crossed_quote`, `not_extreme` or `wide_spread`. The head holds the
  settings in force and the totals.
- **T14 traces its draw and each entry**: the draw (`draw`: the names and the second before the close drawn for each, with
  the seed) and one `entry` per name drawn with its result (`entered` or `skipped:` with `halted`, `restricted`, `no_quote`,
  `crossed_quote`, `no_share`, `no_stop`) and the bid, ask and size it was judged on.

## Consequences

- A whole-market T04 trace is a row per member: a few thousand rows of about a hundred bytes, one trace a day. A null run of
  a hundred seeds writes about twenty small `entry` traces a seed a day; tracing is a choice of the run (E19-S33), not of the strategy.
- A trace is the strategy's own account, not an independent reconstruction: it shows what the strategy believed it was doing.
  That is what the viewer is for; an independent check of a decision is the replay of the log (E19-S37).
- Persisting traces with the results and showing them is E19-S33 to E19-S35.

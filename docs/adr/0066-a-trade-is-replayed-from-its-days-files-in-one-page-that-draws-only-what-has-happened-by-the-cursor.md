# 0066. A trade is replayed from its day's files in one page that draws only what has happened by the cursor

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-189

## Context

The table of a strategy's trades (ADR 0065) says what happened. To judge a trade the user has to see it happen: the price and the
quote around it, when the strategy decided, what the gateway and the broker did with the order and how long that took, what the
trade cost, and why the strategy chose that name and not the next. All of that was kept with the day (ADR 0063, ADR 0064); this
decides how it is put together and shown.

## Decision

- **One page per trade, its data embedded, in the explorer's policy** (ADR 0027): `/research/trade?scenario=&day=&strategy=&n=`,
  GET only and behind the sign-in, served with a policy that lets the page run its own script and lets it neither load nor send
  anything. The data is JSON in a non-executed script block with `<` escaped, so nothing in it can end the block; the page writes
  text only with `textContent` and `createTextNode`. A test pins that the page holds no address it fetches from.
- **The trade is the strategy's `n`th trip of the day** (from 0, in the order of the trips file, the order the table lists them).
- **The orders come from the decision log, joined to the trade by instrument.** The log names instruments by number and the view by
  symbol, so each day now also keeps a host-level trace (`instruments`, strategy 0: the number and symbol of every instrument the
  day made a decision or a fill about). The decisions of the strategy in that instrument from the end of its previous trade in the
  symbol to the end of this one are the trade's; the fills are those of the orders they were accepted as, with the latency from the
  decision to each fill. A gateway refusal is shown with its reason. The accepted decision is the order being sent (the simulated
  gateway accepts and sends in one instant), so the page marks it once, "decision", and the fill after it with its latency.
- **A day kept before this** (no `instruments` trace) shows the trade, costs, market and strategy account and says its orders cannot
  be followed; running the scenario again gives them. Likewise a day run without evidence has no chart, and a strategy that recorded
  no trace says so. Nothing is guessed to fill a gap; what is missing is a note on the page.
- **The cost lines are the trip's own** (gross, fees, borrow, slippage, net, basis points, R). The fees are split into Section 31 and
  the Trading Activity Fee by recomputing them from the sale fills with the scenario's cost model (`sale_fee_parts`, of which
  `sale_fees` is the sum); if the parts do not add up to what the trip paid (a leg the log does not show), only the total is shown and
  the page says why. A position still held when the day's data ended is shown as that, counted at the last trade price, with the fee a
  sale would pay.
- **The chart is the market kept around the trade** (quotes as steps, trades as points, halts as marks), at most 4,000 points of each
  kind: every event within two seconds of the trade is kept, the rest thinned evenly (the last of each group), and the page says how many
  of how many it shows. The default view is the trade and a little either side; two minutes either side and all of the window are a
  choice.
- **The replay cursor draws only what has happened by it.** Quotes, trades and markers after the cursor are not drawn; the order
  rows after it are dimmed; the readout gives the quote, the position held and the open profit before costs (at the bid for a long, the
  ask for a short) at the cursor. Play, pause, a speed (1, 5, 20 or 100 times real time), step to the previous or next event, jump to the
  previous or next marker (decision, fill, exit decision, exit fill, end of day), and replay from the start.
- **The strategy's own account is its trace, as it recorded it:** its head, and its rows, all of them if there are forty or fewer,
  else the first twenty and every row about this symbol, the symbol's row shaded. The library's raw prices are shown as dollars and
  the close as a New York time. For T04 this is the ranked cross-section with the names chosen against the next ones, the filters and
  the names skipped with their reasons.
- **Times are microseconds from the second the trade was entered in** plus that second's New York time, because event time in
  nanoseconds does not fit a number in the page's script.

## Consequences

- The page is only as good as what was kept: a real T04 day's evidence has not been measured (ADR 0064), and its size sets how large
  the page is; the chart's cap bounds the market data in the page at 4,000 quotes and 4,000 trades (about a hundred and fifty kilobytes of text by arithmetic, not measured on a real day; a scripted day's page is 36 kilobytes).
- The page replays what was recorded; it does not rerun the strategy. An independent check of the decision log is E19-S37.
- The day-level replay with every strategy and the budget meters is E19-S36 and builds on the same join.

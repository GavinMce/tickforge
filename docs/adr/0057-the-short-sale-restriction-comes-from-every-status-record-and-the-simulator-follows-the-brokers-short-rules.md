# 0057. The short-sale restriction comes from every status record, and the simulator follows the broker's short-sale rules

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-160

## Context

The decoder read a restriction only from a status record whose *action* was "short-sale restriction change", and the
canonical event had no way to say it ended. A restriction carried over from the day before is announced by no change
at all (every status record carries a restricted flag, `Y`, `N` or `~`), so it was missed, and Tier 0 only ever turned
the flag on. The simulator filled any short sale at the bid, restricted or not, accepted a short of a name the broker
would not lend, and charged a borrow fee on every short.

## Decision

- **The end of a restriction is an event** (`StatusKind::ShortSaleRestrictionLifted`, schema v5: a v4 stream cannot say
  it, a test pins that). Tier 0 sets and clears its flag from the two kinds.
- **The decoder reads the flag on every status record** and keeps the last state per instrument with the dense ids
  (`InstrumentMap::short_sale_restricted`), so it carries across files and sessions. An event is made when the state
  *differs*: a restriction carried over shows at the first record of the day that says `Y`; a first `N` says nothing (not
  restricted is how an instrument starts); `~` says nothing. A change action whose flag does not say which way is the
  start of a restriction, as it was always read. A record can now make two events (a resume and a restriction).
- **The simulator** does not fill a short sale while the restriction lasts: every fill of a sell here is made at the
  best bid, which a restricted short sale may not be made at or below. It rests (immediate-or-cancel expires) and is tried
  again when the restriction is lifted. A short sale priced above the bid that a buyer could lift is not modelled, so this
  is pessimistic. A sale of shares held is not a short sale and is not held back.
- **The broker's borrow rules** (`SimBroker::with_borrow_table`, off by default): a short sale of a name that is not easy to
  borrow is refused as Alpaca's API does today, with no locate modelled (a hard-to-borrow locate is E14-S02): hard to
  borrow, not shortable, and not known (never guessed) each with their reason (status 403). The borrow fee follows the
  broker: none on an easy-to-borrow name. The multi-strategy host builds the table from the snapshot's `shortable` and
  `easy_to_borrow` columns when it has them, and asks nothing when it has neither, so every earlier run is unchanged.
- **The daily report lists what the brokers refused, with the reason**, naming short sales.
- **A backtest report that sold short says what is not point in time**: the easy-to-borrow flag is today's list applied to
  the past, survivorship, and no locate or hard-to-borrow cost (`docs/research/data-and-protocol.md`, point in time).

## Consequences

- E19-S06 depends on E09-S02 (the gateway's own short checks: shortable or locate, SSR, halt, spread, run-up), which is
  still open. This story is the simulator side; a strategy skips a restricted short itself (T05, T06 say so), and a
  gateway that also refuses one waits for E09-S02.
- The refusal texts state Alpaca's rule in its own terms, not its wire text, and the status 403 is the one the API is
  understood to answer with; both need paper credentials to check (E18-S11).
- Live, the restriction is now known from the first status record of the day, but a feed that starts after the
  premarket's first records (a reconnect) learns it from the next record of that instrument.
- Old tapes (v4) keep decoding; they only ever said when a restriction began.

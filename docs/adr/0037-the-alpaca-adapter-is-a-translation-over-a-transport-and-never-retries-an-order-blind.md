# 0037. The Alpaca adapter is a translation over a transport and never retries an order blind

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-74

## Context

Orders the gateway accepts (ADR 0012) and the ledger records (ADR 0030) have to reach a broker, and
what the broker says has to come back as ledger inputs. Alpaca's paper account is the first broker.
Two things make this risky: a submission whose answer never arrives may or may not exist, and the
workspace has no HTTP/TLS/WebSocket dependency (the only third-party crate is `zstd`), so how to
reach the network is a decision of its own.

## Decision

- **`tf-alpaca` translates and talks through a `Transport` trait; it opens no connection.** The
  wire format, the stream protocol and the retry rules are all testable without a network or an
  account. The real HTTPS and WebSocket transport is E09-S10 and needs its own ADR on the TLS
  dependency.
- **An accepted intent becomes a limit order, always.** A collar is a limit at the worst price it
  allows. Prices are exact decimals (never floats), rounded to Alpaca's tick (two places from a
  dollar, four below) in the direction that cannot loosen the order: an entry limit never pays more
  or accepts less than the intent; a protective stop never fires later and a target is never
  demanded at a worse price. A stop alone is an `oto` order, with a target a `bracket`; protective
  orders on an IOC or on an order that closes are refused before sending (Alpaca refuses them).
- **A client order id names every order** (`prefix` + our order number). The prefix keeps a new
  ledger from colliding with an old one on the same account.
- **An order with no answer is looked up, never sent again blind.** After a timeout, a dropped
  connection or a 5xx the adapter asks for the order by its client order id. If it is there, it is
  accepted. If not, it is sent once more under the **same** name (a duplicate is refused by
  Alpaca, and that refusal is read as "the first one arrived", then looked up). After two tries
  the result is `Unknown`, and the ledger order is not closed on a guess: reconciliation (E09-S06)
  settles it.
- **The stream is read defensively.** Fills carry an execution id and a running total: a fill
  delivered twice is applied once, and a fill that does not add up to what Alpaca says the order
  has filled is reported (and kept out of the ledger) rather than applied. An order we did not name
  is reported, never guessed at. A fill before any acknowledgement acknowledges first. After a
  restart the tracker is resumed from the ledger's own totals.
- **Protective legs are recognised and not recorded.** A bracket's stop and target are orders the
  gateway never saw; the ledger has no input for their fills. They surface as `LegFill` events and
  the glue says "not recorded". E09-S11 adds the ledger side.
- **Keys never appear in a body, a log or `Debug` output**; they travel as headers added by the
  adapter and the transport is handed the finished request.

## Consequences

- Nothing here has met the real Alpaca API. The fixtures were written from Alpaca's documentation,
  not captured: how legs appear in responses (`legs`, `parent_order_id`), the by-client-order-id
  lookup, and error bodies are the parts most likely to differ. E09-S10 checks them against a
  paper account and corrects the fixtures.
- Alpaca's rules can change; the tick and order-class rules here are the documented ones as of
  this decision.
- Whole shares only: a fractional quantity anywhere in a message is refused.

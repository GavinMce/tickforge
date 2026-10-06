# 0034. The workspace service only reads, and reads the ledger without its lock

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-128

## Context

The workspace app shows balance, budgets, use, day P&L and runs (E17). Its data lives in the order
ledger, which a running engine holds open under a lock (ADR 0030), and in the run store. The app
must be hosted behind sign-in, and the safest thing it can be is unable to act.

## Decision

- **A new crate, `tf-workspace`, with no write path.** It answers GET requests from the ledger and
  the run store. The only other request it accepts is the login form, which changes the caller's
  cookie and nothing else. Every other method on every path is refused with 405, tested for POST,
  PUT, DELETE, PATCH and OPTIONS on every route. No type in the crate can construct an order, a
  budget change or an approval.
- **The ledger is read without its lock and without repair** (`ReadOnlyStore`). Opening it for
  writing would refuse while an engine runs, and repairing would truncate a record the engine is
  still writing. A last record that is incomplete is left out and reported, never removed. Damage
  anywhere else is still an error, and a ledger that does not replay to its recorded decisions is
  a 500 that says so, not a plausible-looking page.
- **Each request replays the ledger.** There is no cache to go stale and nothing to invalidate;
  replay is linear and a day's ledger is small. If that stops being true the cost shows up as
  latency, not wrong answers, and a cache can be added then.
- **Money is text** (`"1234.57"`), because event times and raw amounts exceed what a JSON number
  holds exactly in most clients.
- **Sign-in is one shared token** of at least 16 plain characters, read from a file: as a bearer
  header for programs, or typed into a login page that sets an HttpOnly, SameSite=Strict cookie.
  Comparison does not stop at the first differing byte. `/health` is open. Default bind is loopback.
- **What this is not.** It speaks plain HTTP, has no per-user identity, no rate limit on failed
  sign-ins and no TLS. The token is the cookie value, so it must only ever cross a TLS link.
  Hosting it for real (TLS, identity provider, deployment) is E17-S17, after E13-S02, and is not done here.

## Consequences

- The UI stories (S10–S13) have one stable JSON surface to build on.
- Exposing it beyond loopback is a decision someone has to make on purpose; the command warns.
- The server handles one connection at a time with a five-second read timeout, so a slow client
  delays others. That is acceptable for one person on loopback and is why a proxy belongs in front.

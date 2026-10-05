# Architecture decision records

An ADR records one decision that is expensive to reverse, and why it was made,
so the reasoning outlives the conversation. Write one when a change:

- picks a language, store, protocol or wire format;
- adds a rule other code must follow (or relaxes one, such as allowing `unsafe`);
- trades something away that a later reader would otherwise "fix".

How:

1. Copy [`0000-template.md`](0000-template.md) to `NNNN-short-title.md` with the next number.
2. Fill it in. Keep it to a page; link code and other ADRs instead of repeating them.
3. Open it as a normal PR (`TIC-n: ...`). The PR is the review.
4. Never edit an accepted ADR to change the decision. Write a new one that
   supersedes it, and set the old one's status to `Superseded by NNNN`.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-rust-for-the-hot-path.md) | Rust for the hot path | Accepted |
| [0002](0002-no-redis-on-the-hot-path.md) | No Redis on the hot path | Accepted |
| [0003](0003-determinism-rules.md) | Determinism rules | Accepted |
| [0004](0004-versioned-event-encoding.md) | Versioned event encoding and event identity | Accepted |
| [0005](0005-security-master-identity.md) | Security master: stable ids, dated tickers, dated provider keys | Accepted |
| [0006](0006-raw-tape-format.md) | Raw tape format and the zstd dependency | Accepted |
| [0007](0007-paced-replay-waits-inside-poll.md) | Paced replay waits inside `poll`, on an injected pacer | Accepted |
| [0008](0008-benchmarks-are-informational-and-tracked-per-commit.md) | Benchmarks are informational and tracked per commit | Accepted |
| [0009](0009-run-manifests-and-content-addressed-results.md) | Run manifests and results addressed by the manifest hash | Accepted |
| [0010](0010-strategies-are-event-driven-and-cannot-touch-the-outside-world.md) | Strategies are event-driven and cannot reach the clock or the outside world | Accepted |
| [0011](0011-simulated-broker-fills-against-recorded-quotes.md) | The simulated broker fills against recorded quotes, with latency | Accepted |
| [0012](0012-risk-gateway-decides-and-limits-are-fixed.md) | The risk gateway decides every order, and its limits are fixed | Accepted |

# 0015. Strategy 1, long side: classify on features, wait for the bounce, trail the high

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-65

## Context

The first strategy trades low-float runners: buy a healthy pullback, ride the
continuation. It must be rule-based with every threshold a parameter, and it must
pass on the synthetic healthy scenario and stay out of the dangerous one.

## Decision

`tf-strategy::momentum::MomentumLong` with a typed, validated `MomentumParams`
(no numeric constants in the logic):

- **Scan** from Tier 0 windows (spike size and volume, price and spread filters);
  a hit promotes the symbol into the strategy's own bounded `Tier1`.
- **Judge** once a second from the pullback features: dangerous if too deep or
  volume has not dried up (stop watching, cool down); healthy if deep enough to be
  a real pullback, near enough to the high, volume dried, enough higher lows and
  bid support. Drop it if it has not turned healthy within the window.
- **Enter** with an IOC collar around the ask and a broker-side stop under the
  pullback low, sized from a notional budget. **Exit** on a trailing stop from the
  high since entry or a maximum hold; the strategy tracks positions from final
  order updates, so partial fills and failed entries are handled.
- **`min_higher_lows` defaults to 1.** With 0 the strategy buys mid-pullback and is
  stopped out on every synthetic seed (about -$30 per $1,000 trade); with 1 it
  waits for the bounce. A test keeps that trade-off visible.

## What was measured, and what it does not show

Over 60 synthetic seeds the defaults entered the healthy scenario 58 times and the
dangerous scenario never. The two misses never produced a healthy reading. The
healthy runs make money in the simulator (the continuation phase is +30% or more)
but that is a property of the generator: it is not evidence of an edge. The
thresholds were chosen on these scenarios and need to be re-derived on recorded
data before they mean anything. The strategy is not yet wired through the risk
gateway in an end-to-end loop, and the short side (E08-S04) is separate.

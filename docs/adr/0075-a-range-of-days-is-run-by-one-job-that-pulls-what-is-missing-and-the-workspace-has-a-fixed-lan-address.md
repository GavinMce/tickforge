# 0075. A range of days is run by one job that pulls what is missing, and the workspace has a fixed LAN address

- Status: Accepted
- Date: 2026-10-09
- Jira: TIC-185

## Context

The backtest view (ADRs 0065 to 0067) shows finished runs and cannot start one, and nothing on the cluster produced a run: the
prepare job certifies one day, which is a gate for the live day and not a backtest (a strategy that lost its hard limit on the day
is refused there). To see a strategy's trades over a period, a person had to pull each day, make snapshots and run `tf research run`
by hand. The plan now includes the last twelve months of `EQUS.MINI` history (a quote for `tbbo` from 1 January 2026 to 9 October 2026
is nothing; one reaching back to 1 October 2025 is $12.44), a day being about 220 MB stored. The workspace was reachable only through
a port-forward or by the https name through Traefik, which needs a hosts-file entry.

## Decision

- **`scripts/research-range.sh NAME FROM TO` is the whole path for a range:** each weekday not stored is pulled and given its names
  (ADR 0074), the reference is fetched from 75 days before the range, one snapshot is made per trading day, the store is indexed, and
  `tf research run` writes the scenario `NAME` under the research directory the workspace reads, with the market around each trade
  kept (`--evidence`) so the trade page can draw it. A holiday is a day the pull stores nothing for: it is said and left out. A pull is
  refused above `$MAX_COST`, one dollar a day by default, because the plan should make it nothing; a charge is a surprise to stop on.
- **It uses its own reference and snapshot directories** (`/data/history/ref`, `/data/history/snapshots`), not the live day's, so it can
  run while the prepare job does and cannot change what a live day reads.
- **`scripts/backtest-dev.sh NAME FROM TO` starts it as a Job on the cluster** from the image the workspace runs, with the history
  volume and the deployed strategy set, follows the log, and says where to look. The Job has no live volume and no deadline beyond six
  hours, and does not retry (the run continues from the first unfinished day when started again under the same name). The viewer still
  only reads (ADR 0065): starting a run is a command, not a button.
- **The set that is run is the deployed one,** and a scenario is one configuration (ADR 0059), so another set is another name.
- **The workspace has a `LoadBalancer` Service on a fixed address of the MetalLB pool,** `10.0.30.43` port 8787, beside the https name.
  It is plain HTTP behind the sign-in token, on the LAN only; the token therefore crosses the LAN unencrypted.
- **The cost model's Section 31 table is known through 11 December 2026.** The first range run stopped at 1 October: the table ended
  with the fiscal year, and a date past it is refused, not given the nearest rate. The SEC's advisory
  [2026-2](https://www.sec.gov/rules-regulations/fee-rate-advisories/2026-2) keeps $20.60 until 60 days after legislation sets the
  Commission's fiscal year 2027 appropriation. The only law so far is the continuing resolution P.L. 119-103 (signed 2 September 2026),
  which funds the government to 11 December and does not set it (the table's own history shows the fiscal year 2025 resolution did not
  change the rate either), so $20.60 is known through that day. It has to be looked up again after it.

## Consequences

- A strategy can be run over a month of real days with one command and looked at in the workspace.
- A year is not what a volume of 60 Gi holds (about 220 MB a day stored); a month or two is. Grow the volume before pulling more.
- Only the templates the set file can name (`t04`, `t14`) can be run this way; a new strategy is code in the image.

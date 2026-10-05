# 0027. The explorer replays stored runs and checks them first

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-114

## Context

`tf backtest --store` keeps a manifest and integer metrics, not the tape, the orders or the
trades. To look at a stored run the explorer has to get those back, and what it shows must
be the stored run, not whatever the current code does with the same inputs.

## Decision

`tf explore HASH [HASH2] --store DIR [--rules FILE]...` rebuilds the run from its manifest,
replays it, and refuses to show it unless the replay matches what was stored:

- The command line is reconstructed from the manifest, then the manifest is rebuilt from that
  command line and compared key by key (config, parameters, data range). Any difference, or a
  value that is not a whole number in its unit, is an error (the rebuild is exact or refused).
- The replayed tape's event count and hash, and every metric, must equal the stored ones, in
  both directions (no missing or extra metric).
- Rule sets are in the manifest by fingerprint only; the file must be given again and is found
  by fingerprint.
- Momentum single runs only. A/B runs and the trend strategy are refused with the reason.

With two runs of one session (same tape hash) the page also holds a trade-by-trade comparison,
computed in Rust (`tf_backtest::compare`): trades pair by overlapping holds on one symbol, then
by symbol alone; a pair is same or changed (naming what differs), and an unpaired trade says
what the other run did with that symbol before then.

The page is the viewer with the data embedded (`tf explore --out`, or `tf backtest --export
FILE.html`); the same viewer fragment is what gets published as an artifact.

## Consequences

- A stored run stops opening when the code changes its behaviour. That is the point: it says
  so, with the first metric that moved and the git revision it was stored at.
- Each run in a comparison carries its own copy of the tape, so a comparison page is about
  twice the size of a single one.
- Agent-proposed rule edits and a run browser remain for later stories.

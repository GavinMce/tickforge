# 0071. Research and the live day read one strategy set, and research runs a store day by day

- Status: Accepted
- Date: 2026-10-08
- Jira: TIC-185

## Context

The strategies existed as library functions called from tests; nothing named a variant, its parameters, its universe and its budget
in a file, and nothing ran a history store through the research harness from a command. A live month needs the same variants as
the research that chose them, defined once, or the thing tested is not the thing traded.

## Decision

- **A strategy set is one text file** (`strategy set v1`, `tf_host::set`): the balance the budgets divide, the loss limits (soft and
  hard, in basis points of a strategy's own budget), the gateway's limits, and a line for each variant: a number (the one in its
  intents), a name (its id in the budget tree and in results), a template (`t04`, `t14`), the template's parameters (any not given
  are its defaults, an unknown one is refused), a universe file, a priority and optionally a share. Shares not given are the rest of
  the whole split evenly. The set gives a host configuration (limits, one budget group `g`, the engine's defaults) and the definitions,
  and refuses what the host would refuse (a gross limit under the order limit, shares over the whole, a name the tree will not take)
  when it is read, not when the day starts. Research and the live day both read it.
- **The templates are the library's** and a variant's identity is still its definition's fingerprint (ADR 0060): the same line twice is
  one variant, another parameter another.
- **`tf research run` runs the set over the days of a store** (`StoreSource`, a `DaySource` over one dataset and schema, a date range
  and an optional subset of symbols) into a results directory, each day as a live day is run (ADR 0068), taking each day's reference
  snapshot from a directory of `<date>.snapshot` files. A day is identified by its file's checksum, its snapshot and the subset, so a
  day made from other data is made again. **A snapshot that is not as of a session before the day is refused**, since one built from
  the day's own bars would tell a strategy the close in the morning. `tf research snapshots` makes the files from daily bars, one for
  each trading day, as of the last session before it; `tf research show` says what a directory holds.
- **What is not here:** the columns that need minute history (previous high and low, ATR, volume baselines, the hourly EMA) are not
  in these snapshots, so a strategy that needs them refuses to run until they are (E19-S04's builder is not looped per day yet);
  the templates are T04 and T14; the first run over real days has not been made.

## Consequences

- A variant for the live month is one line in a file that research has already run over history.
- The reference snapshot of each day is a file on disk (about the size of the day's symbol list): a twelve-month run keeps two
  hundred and fifty of them.
- Per-strategy universes differ only by file; one set can hold variants over different universes.

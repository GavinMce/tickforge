# 0074. A stored day's instruments are named from the vendor's symbology, kept beside the day's file

- Status: Accepted
- Date: 2026-10-09
- Jira: TIC-185

## Context

The first prepare job that got as far as certifying on real data (EQUS.MINI `tbbo`, 8.45 million records for 8 October) failed with
`UnknownSymbols(["A", "AA", ...])`. A history pull asks for `ALL_SYMBOLS`, and for that request Databento's DBN metadata carries no
symbol mappings (measured on a minute of the real feed: 2,959 instruments seen, none named). A live session is named by the gateway's
symbol-mapping records; a stored day has only the vendor's numbers. The replay numbers instruments in the order first seen and learns
names from the file, so on a pulled day it knew every instrument and no symbol, and every strategy naming symbols was refused.

The vendor's symbology service answers the question for a day and a dataset (`symbology.resolve`, `stype_out=instrument_id`; free; for
EQUS.MINI on 8 October 13,320 symbols, covering every number the tape carried).

## Decision

- **The names are a file beside the day's file:** `<date>.names` next to `<date>.dbn.zst`, one `ID SYMBOL` per line (comments with `#`).
  A line that is not that is an error that stops the replay and names the file, so a cut file does not pass for a short one. A day
  with no names file is replayed as before (a capture, or a pull that named symbols, needs none).
- **The capture reader gives them to the decoder** (`Decoder::with_names`, after the ids carried from the day before): a name is applied
  when its instrument is first seen and never makes one, so the numbering is unchanged. They win over the file's own metadata, which is
  the later and more specific information when both exist.
- **`tf history names DIR --dataset --schema --date --symbology FILE`** writes the file from a `symbology.resolve` result (the symbols
  in force on that day, one to a number), by writing a `.part` and renaming. `scripts/store_names.sh` fetches the symbology (with
  retries) and runs it; `prepare-day.sh` calls it for the day it certifies on, and so does anything else that pulls a day to replay.
- **The history manifest does not list the names files** and `verify` does not check them: they are small, free to make again, and a
  wrong one shows at once as symbols refused or a strategy trading a name it should not. Revisit if a run is ever reported without
  them.
- **A replay of a stored day selects only from the names the day shows.** The snapshot of the whole market (13,452 rows on 8 October)
  names thousands that did not tick that day; they cannot trade on it, and a universe selecting one was refused as unknown, so the
  first certification still failed after the names resolved. `certify_files` and the research day cut the snapshot to the symbols
  the tape names (`replay::on_tape`). A live day is not cut: the gateway names everything it subscribed. `replay_files`, which
  compares against a live log made with the full snapshot, is not cut either.

## Consequences

- A pulled day can be certified on and researched over; the first real certification can be tried.
- A ticker that changed on a day is named as it was that day, because the symbology is asked for that day.
- A day pulled before this change has no names until `scripts/store_names.sh` is run for it.

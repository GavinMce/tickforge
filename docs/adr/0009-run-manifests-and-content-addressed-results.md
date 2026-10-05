# 0009. Run manifests and results addressed by the manifest hash

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-34

## Context

A run is a pure function of its inputs (ADR 0003), and the plan is to execute
thousands of them as Jobs (backtests, parameter sweeps, walk-forward). Re-running
what has already been run wastes the cluster, and a result that cannot be traced
back to exactly what produced it is not worth keeping.

## Decision

- A **manifest** (`tf-manifest`) records everything that determines a run: git
  sha, kind of run, seed, config, strategy parameters and the data range. The
  parameters also get their own hash, to group runs that share them.
- Its **key is the SHA-256 of a canonical byte encoding**: fixed field order,
  little-endian integers, length-prefixed strings, maps in key order, so the key
  does not depend on platform or on the order fields were added. The encoding is
  pinned by a test (also computed independently in Python), and changing it needs
  a `SCHEMA` bump.
- SHA-256, not the FNV used for goldens: a key collision would return one run's
  results for another, silently. It is implemented in-crate (about 70 lines,
  checked against NIST vectors and coreutils at every padding boundary) to avoid
  a dependency. As defence in depth a lookup also compares the stored manifest
  and reports a mismatch as a collision.
- A **result** is integers only (event count, event-stream hash, named integer
  metrics), in a line-based text form that embeds its manifest. It is stored as
  one file per manifest, `<aa>/<hash>.tfrs`, written whole and renamed into place.
- **Dedupe**: `get_or_run` returns a stored result without running. Storing a
  result when one exists succeeds only if they are identical. **A differing
  rerun is an error (`Mismatch`)**, not an overwrite: runs are deterministic, so
  a difference means something is not.
- The git sha carries `-dirty` when the working tree has changes, so results from
  uncommitted code never share a key with the commit.

## Consequences

- Reruns are free, sweeps are cacheable, and every result says exactly how it was
  made. Any change to code (new commit) re-keys results; that is intended, since
  new code may behave differently.
- Results are only as trustworthy as the manifest is complete: anything that
  influences a run but is not in the manifest (an environment variable, a data
  file's contents rather than its name and range) breaks the guarantee. Callers
  must put it in config, or in the data source's name.
- The result body has no checksum of its own; the stated manifest hash is
  verified on read, so a damaged manifest is caught but an edited metric is not.
- The store is a directory. A shared object store (E04-S03) can replace it behind
  the same `get` / `put` / `get_or_run` shape.

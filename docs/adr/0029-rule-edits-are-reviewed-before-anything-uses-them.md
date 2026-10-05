# 0029. Rule edits are reviewed before anything uses them

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-117

## Context

Entry rules are data (ADR 0026), so an agent can be allowed to propose a different rule set.
Something has to stand between a proposal and use: a way to see what changed, evidence about
what it does, and a record of who decided.

## Decision

`tf rules` is the review path. Nothing it does changes anything live.

- **diff:** a structural diff of two rule sets (`tf_strategy::rule_diff`). A change to a
  protective stage (`too_old`, `dangerous`) that makes it fire less often is flagged as
  loosening: a condition removed, a threshold moved the permissive way (compared as integer
  inequalities, so `>= 351` equals `> 350`, with parameters read at their default values), or
  `any` made `all`. Changes to `armed` and `enter` are shown but not flagged; their effect is
  in the outcomes.
- **review:** base and candidate are backtested on the same held-out sessions (seeds
  1000..1023 by default, which a proposer's own tuning should not use). Every run goes in the
  store through the same manifests as `tf backtest --store`, so `tf runs` and `tf explore`
  open and compare them, and the rule files are kept in the store under their fingerprints so
  they need not be passed again. Gates: the candidate differs; enough sessions and trades; no
  more entries in the dangerous scenarios than the base; net P&L and the worst session
  drawdown no worse than the base's (tolerances are flags); not worse in more than a quarter of
  sessions. A failed gate rejects. A candidate that passes but loosens a veto is
  `needs-human`; otherwise `accepted`.
- **record:** an append-only text file per proposal (candidate and base fingerprints): the
  proposer, reason, verdict, every gate with its reading, the changes, the stored runs. A
  record whose verdict its gates do not support, or a rejected one carrying an approval, does
  not parse.
- **approve:** a named person's approval is a line appended to the record. A rejected proposal
  cannot be approved. Acceptance by the gates is not approval: a person is always the last step.
- A candidate already reviewed against the same base is refused, so a verdict and its
  approvals are not overwritten.

## Consequences

- The gates test internal consistency on synthetic sessions: no worse on the sessions given.
  An edit that changes nothing those sessions can show is accepted, with a note saying so.
  They are not evidence of an edge, and a proposer who can see the review seeds can overfit
  them.
- Nothing yet reads approvals to switch rules on; that is for the component that deploys
  rules. The agent interface (E12) will call these commands.
- Policy tolerances are chosen by whoever runs the review; a stricter default is a decision
  for later, with real data.

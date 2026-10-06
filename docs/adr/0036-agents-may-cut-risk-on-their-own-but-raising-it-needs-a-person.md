# 0036. Agents may cut risk on their own, but raising it needs a person

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-133

## Context

Agents will propose budget changes (E17, E12). Budgets are how risk is reserved and bounded
(ADR 0031), so an agent with a free hand over them is an agent with a free hand over risk. The two
directions are not alike: cutting a budget cannot make a loss bigger, raising one can. The same
proposal path (ADR 0029 for rules) needs a policy for which is which. The agent tools
(E12-S06) are not built yet, so the policy and the records come first, behind a command line that
a tool layer can wrap later.

## Decision

- **A pure policy** (`tf_proposals::decide`) judges a proposed tree against the budgets in force:
  - It must pass `tf_budget::check_edit` first (same groups and strategies, children within their
    parent, no strategy cut below what it has in use). Otherwise it is **refused**.
  - A change that raises any share, or loosens either loss limit (a mixed change counts as
    loosening), **needs a person**.
  - An increase for a strategy in **drawdown** (or for a group holding one) is **refused**, not
    left for a person: wait until it recovers. Drawdown means stopped by a loss limit now, or net
    down over its last three sessions taken together (zero is not down).
  - A change that only reduces risk applies **on its own** if no share is cut by more than the
    step (default 10 points) and no node was changed by an applied or approved proposal within the
    cooldown (default a day). Larger or sooner goes to a person, with every reason listed.
  - A randomised test checks the safety property directly: whatever the policy applies on its own
    raises no strategy's dollar budget and no loss amount.
- **"Applies on its own" means queued, not enacted.** It goes into the ledger's inbox (ADR 0035)
  and takes effect at the next rebalance (ADR 0033), exactly like a person's edit. The policy never
  moves money or touches the ledger.
- **Everything is recorded** as files in `proposals/` beside the ledger: who, when, why, the
  evidence, the policy in force, the verdict and its reasons, the nodes changed and the tree.
  Records are never edited. A person's approval or decline is a separate file with the proposal's
  number, created exclusively, so a proposal is decided once.
- **Approval checks again.** A person approving does so against the budgets and drawdown as they
  are now. The agent's step and cooldown do not bind a person; the rules and the drawdown refusal
  still do. If the approval cannot be queued the decision is taken back.
- **`tf budgets propose|proposals|approve|decline`.** A refused proposal makes `propose` exit with
  an error so an agent notices.

## Consequences

- Pending proposals queue behind a person; nothing times out. A stale one is declined or
  approved against the budgets of the day it is approved.
- Two proposals queued before the engine applies the inbox: the later request replaces the earlier
  scheduled tree (ADR 0035), so a person approving an old proposal can overwrite a newer cut. The
  panel (E17-S15) shows what is scheduled so that is visible; serialising them is future work.
- Drawdown is judged from the ledger's own sessions, so it needs the strategies to have traded
  there. A strategy with no history is not in drawdown.

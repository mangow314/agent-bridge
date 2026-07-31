# Review Overlay — cross-vendor verification rounds

## Verifier hard-condition contract

When dispatching any independent review (code-review, codex-rescue, subagent reviewers, or a pane worker), the prompt MUST include:

- **Reject on hard conditions only**: test results, data checks, point-by-point spec comparison, reproducibility — signals that are machine-judgeable or independently re-runnable.
- **A rubric carried in the brief is a hard condition**: its criteria are the spec for that dispatch, so the verifier judges them point by point and rejects on a miss, even when the subject is taste (API shape, doc quality, naming). This is how a "human judgment" plan gate becomes checkable. The verifier's only precondition is answerability, enforced per item: every criterion must be settleable yes/no from evidence, and any criterion that is not makes the rubric defective — the verifier blocks naming that criterion instead of dropping it and judging the rest. Rubric length is the plan author's call, never grounds for rejection.
- **NEVER reject for style, narrative, or opinion *of its own***: unbriefed taste goes only into a "suggestions (non-blocking)" section of the report.
- The maker's "done" is a claim, not proof: every verdict needs evidence (file:line, test output, re-run result).
- The verifier judges but never edits (write/review separation); fixes belong to the maker / main thread.
- **Security-sensitive escalation**: if the diff touches auth/permissions/crypto/secrets handling, hooks/safety chain, or agent definitions, the verifier dispatch MUST request maximum thoroughness (full adversarial re-run of the relevant checks), regardless of diff size.

## Review vehicle (which codex to call)

- **Default vehicle: an agent-bridge pane worker** (`--runtime codex`). Mechanics — the spawn/send/await/read sequence, retention vs evict/despawn — follow the agent-bridge skill and its `share/orchestrator-brief.md`; do not restate them here. Review-specific overlay: first-pass review rounds run at the runtime's default tier/effort — review precision holds at lower effort on current frontier models; escalate tier/effort only for security-sensitive diffs or a round that failed to converge, and name the escalation reason in the dispatch. Residual context stays in a live pane for follow-up questioning, and every round dogfoods the bridge.
- **Fall back to codex-rescue subagent / codex MCP only when agent-bridge is unavailable**: `agent-bridge` not on PATH, no tmux server running, the current sandbox blocks the tmux socket, or the spawn cap is full and no existing worker can be safely reclaimed. State the fallback reason in the report.
- **"獨立 worker" in a user instruction means an agent-bridge spawned worker** — a subagent or MCP call does NOT satisfy it. When unsure which vehicle the user meant, the pane worker is the default reading.
- **Known limit — codex pane workers cannot re-run this repo's test suite** (observed 2026-07-28, two rounds): the codex sandbox denies creating the suite's dedicated tmux socket (`Operation not permitted`), so `tests/run-tests.sh` dies early (`PANES[0]: unbound variable`). Consequence for every review round: suite numbers (e.g. "N PASS / 0 FAIL") are always maker claims, never independently re-run by the reviewer; scope the dispatch as code review + statically checkable evidence, and have the reviewer mark test-result verdicts as unverifiable-by-vehicle rather than confirmed.

## Plan-stage second opinions (codex + agy)

A **plan-stage** second opinion is a design input, not a diff verification. It is
therefore **outside** the per-diff round cap in `~/.claude/rules/review-discipline.md`
— that cap governs verification rounds on a diff (subagent verify + one cross-vendor
codex round). Do not count plan-stage opinion rounds against it, and do not use the
cap as a reason to skip one.

- **Two vendors by default when the design space is wide**: one `--runtime codex`
  worker and one `--runtime agy` worker, each given the same plan and asked
  independently — ask them before they see each other's answer, or the second
  opinion collapses into agreement with the first.
- **Headless stays available**: a one-shot opinion needs no pane worker at all —
  `codex exec …` / `agy -p '…'` from the main session is the cheaper path. Reach for
  a pane worker when you want the opinion to survive across turns (follow-up
  questioning, multi-round argument) or want both vendors working in parallel.
- **agy worker caveats** (measured in `docs/agy-probe.md`): agy has no hooks
  subsystem, so it never writes `state/<name>.json` and notifications always take the
  legacy send-keys path. Nothing in send/receive/reply depends on the state channel,
  so the worker contract is unaffected — but do not expect `notify-deferred` behavior
  from an agy worker.

### Orchestrator's arbitration authority (user directive, 2026-07-31)

The orchestrator running the plan stage **may drive the two vendors against each
other**: relay A's objection to B for rebuttal, iterate over several rounds, and
decide when to stop. When the rounds do not converge:

- **the orchestrator rules on it** — stating which position it adopted and why — **or**
- **escalates the disagreement to the user as concrete questions**, each stating what
  each vendor claimed and what turns on the answer.

What is not allowed is letting a live disagreement pass silently into the plan.
Whichever exit is taken, record it where the plan lives, so the decision is auditable
later.

Referenced from ~/.claude/rules/review-discipline.md; SKILL.md pointer to be added by the repo owner.

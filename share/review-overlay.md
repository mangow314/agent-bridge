# Review Overlay — cross-vendor verification rounds

## Verifier hard-condition contract

When dispatching any independent review (code-review, codex-rescue, subagent reviewers, or a pane worker), the prompt MUST include:

- **Reject on hard conditions only**: test results, data checks, point-by-point spec comparison, reproducibility — signals that are machine-judgeable or independently re-runnable.
- **A rubric carried in the brief is a hard condition**: its criteria are the spec for that dispatch, so the verifier judges them point by point and rejects on a miss, even when the subject is taste (API shape, doc quality, naming). This is how a "human judgment" plan gate becomes checkable. The verifier's only precondition is answerability, enforced per item: every criterion must be settleable yes/no from evidence, and any criterion that is not makes the rubric defective — the verifier blocks naming that criterion instead of dropping it and judging the rest. Rubric length is the plan author's call, never grounds for rejection.
- **NEVER reject for style, narrative, or opinion *of its own***: unbriefed taste goes only into a "suggestions (non-blocking)" section of the report.
- The maker's "done" is a claim, not proof: every verdict needs evidence (file:line, test output, re-run result).
- **Every finding carries exactly one evidence lineage tag** from the closed enumeration in `~/.claude/rules/review-discipline.md`: `spec` / `rerun-output` / `artifact-state` / `diff` / `prior-report` / `model-prior`. A `model-prior` finding never blocks; two findings sharing a tag are one failure domain and count as one vote, however many reviewers raised them.
- The verifier judges but never edits (write/review separation); fixes belong to the maker / main thread.
- **Security-sensitive escalation**: if the diff touches auth/permissions/crypto/secrets handling, hooks/safety chain, or agent definitions, the verifier dispatch MUST request maximum thoroughness (full adversarial re-run of the relevant checks), regardless of diff size.

## Reference codes in review reports

Long review outputs number their findings with reference codes (F1, F2, …;
D1 for design items). Hard limits:

- **Single-report scope**: a code is meaningful only inside the report that
  defined it — never carry F1 across reports or sessions as if it were a
  global identifier.
- **Cross-worker merges add a worker prefix**: when the orchestrator merges
  findings from more than one reviewer, every code gets the worker's name
  prefixed (e.g. `codex:F1`, `verifier:F1`) to prevent collisions.
- Reference codes stay in review artifacts; they never enter global
  output-style prose (compressed codes fight the plain-language style).

## Review vehicle (which codex to call)

- **Default vehicle: an agent-bridge pane worker** (`--runtime codex`). Mechanics — the spawn/send/await/read sequence, retention vs evict/despawn — follow the agent-bridge skill and its `share/orchestrator-brief.md`; do not restate them here. Review-specific overlay: first-pass review rounds run at the runtime's default tier/effort — review precision holds at lower effort on current frontier models; escalate tier/effort only for security-sensitive diffs or a round that failed to converge, and name the escalation reason in the dispatch. Residual context stays in a live pane for follow-up questioning, and every round dogfoods the bridge.
- **Fall back to codex-rescue subagent / codex MCP only when agent-bridge is unavailable**: `agent-bridge` not on PATH, no tmux server running, the current sandbox blocks the tmux socket, or the spawn cap is full and no existing worker can be safely reclaimed. State the fallback reason in the report.
- **"獨立 worker" in a user instruction means an agent-bridge spawned worker** — a subagent or MCP call does NOT satisfy it. When unsure which vehicle the user meant, the pane worker is the default reading.
- **Known limit — codex pane workers cannot re-run this repo's test suite** (observed 2026-07-28, two rounds): the codex sandbox denies creating the suite's dedicated tmux socket (`Operation not permitted`), so `tests/run-tests.sh` dies early (`PANES[0]: unbound variable`). Consequence: the pane worker is a pure **consumer** of evidence, never its producer. The dispatcher (main thread or a `test-runner` subagent) runs the commands and hands over an evidence pack; a criterion with no artifact in the pack's manifest is marked UNVERIFIABLE-BY-VEHICLE by the reviewer, never inferred and never back-filled from the diff.

## Evidence-grounded second round (the cross-vendor diff round)

The cross-vendor round changes the **evidence source**, not only the vendor. Two models
reading one diff are two interfaces onto a single upstream lineage; measured, that setup
still approves 81.7% of unsafe proposals when the shared upstream is stale, while an
independent source drops pooled false approval 62.9% → 22.9% (arXiv:2609.10969 Table IV).
So the round below never receives the diff in its first phase.

**Evidence pack** (built by the dispatcher before spawning the worker):

```
<evidence-dir>/
  spec.md        # acceptance criteria copied verbatim from the original brief
  manifest.md    # one row per artifact: path | exact command | exit code | timestamp | git HEAD sha | worktree clean/dirty
  *.log          # raw untruncated stdout+stderr, one file per command
  *.snapshot     # post-change rendered output / behavior snapshot, where a log is not the right form
```

Criteria with no runnable check are listed in `manifest.md` as `no artifact` with the
reason. The worker reports them UNVERIFIABLE-BY-VEHICLE.

**Sealed two-phase.** Phase A is evidence-only: the worker replies with its phase-A verdict
and must not revise it afterwards. Only then does the dispatcher send the diff for phase B,
which is appended as a separate section and whose findings all carry `diff` lineage.

**The seal is discipline, not enforcement.** Nothing stops a worker from reading the diff
during phase A, and there is no way to detect that it did. The only trace is that the
phase-A reply has already landed in the bridge mailbox and cannot be rewritten afterwards —
that constrains revision, not what the worker read. A phase-A verdict edited after the diff
arrives is void and the round is recorded as diff-lineage only; a phase-A verdict that
quietly peeked is indistinguishable from an honest one. Treat this the way
`~/.claude/rules/safety.md` treats the string guard: a guardrail, not a sandbox — never
cite the seal as proof the round was source-independent.

### Codex second-round brief template (copy verbatim, fill the `<>` slots)

```
You are the evidence-grounded second verification round. This is PHASE A.

Requirement under test (verbatim):
<paste spec.md, or its path: <evidence-dir>/spec.md>

Evidence pack (read these paths, nothing else):
<evidence-dir>/manifest.md   — how each artifact was produced
<evidence-dir>/*.log         — verbatim command output
<evidence-dir>/*.snapshot    — post-change rendered output / behavior snapshot

ALLOWED INPUTS: the requirement text above, and the files in the evidence pack.
FORBIDDEN INPUTS (hard): the diff or any reconstruction of it, `git diff` / `git show` /
`git log -p`, the pre-change file contents, the maker's report, the first round's report,
and any paraphrase of those. If you cannot answer without one of them, say so and mark
the criterion UNVERIFIABLE-BY-VEHICLE — do not go and read it.

You cannot run this repo's test suite (sandbox limit). Do not attempt it. Judge only from
the artifacts you were given; a criterion with no artifact is UNVERIFIABLE-BY-VEHICLE, not
a pass and not a fail.

Your question is NOT "is this change correct". It is: does the observed behavior in the
evidence pack satisfy the requirement above, criterion by criterion?

Reject on hard conditions only — output mismatch, criterion-by-criterion comparison,
reproducibility. Never reject for style, narrative, or your own taste; unbriefed taste
goes to "suggestions (non-blocking)".

The criteria above are the spec for this dispatch: judge them point by point and reject on
a miss, even when the subject is taste (API shape, doc quality, naming). Your one
precondition is ANSWERABILITY, enforced per item: every criterion must be settleable
yes/no from the evidence you were given. A criterion that is not settleable that way makes
the rubric defective — STOP, return a blocker naming that exact criterion, and do not
silently drop it and judge the rest. Criterion count is the author's business: never
reject a well-formed rubric for being short.

You JUDGE BUT NEVER EDIT: read-only. No file writes or edits, no `git add` / `commit` /
`stash` / checkout or any other state change, no installs. Fixes belong to the maker. If a
check would need state-mutating setup, report that as a blocker instead of doing it.

Tag every finding with exactly one evidence lineage tag:
  spec | rerun-output | artifact-state | diff | prior-report | model-prior
`model-prior` findings never block. Two findings sharing a tag are ONE failure domain and
count as one vote. The list is closed — a finding fitting none of them is defective:
raise it as a blocker instead of inventing a tag.

<If the change touches auth/permissions/crypto/secrets, hooks/safety chain, or agent
definitions, add: This is a security-sensitive change — apply maximum thoroughness to
every artifact bearing on it.>

Reply format (≤2K tokens, Traditional Chinese):
- Overall phase-A verdict: CONFIRMED / REFUTED / BLOCKED (any UNVERIFIABLE criterion caps
  the overall verdict at BLOCKED)
- Per criterion: verdict + lineage tag + evidence (artifact path + verbatim excerpt)
- Findings numbered F1, F2, … each with its lineage tag
- Suggestions (non-blocking)
Then STOP and wait. Phase B will be appended separately. Nothing prevents you from reading
the diff early and nothing would detect it — which is exactly why this round is worthless
if you do. Do not go looking, and do not revise this verdict once it is sent.
```

Phase B, sent only after the phase-A reply has landed:

```
PHASE B. Your phase-A verdict is sent and stands as given — do not edit or reinterpret it.
Here is the diff: <path or paste>.
Append a separate phase-B section: findings visible only in the change itself, each tagged
`diff`. Do not restate phase-A findings. If the diff contradicts your phase-A verdict, say
which phase-A criterion it contradicts and why, without rewriting phase A.
```

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
- **Independence is a brief clause, not a property of the mailbox.** Every worker can read `~/.local/share/agent-bridge/tasks/`, so a second-opinion worker can see the first one's `response.md` (observed 2026-09-28: an agy worker read the codex worker's reply before answering). The dispatch brief for a plan-stage opinion MUST say: do not read any task directory under `~/.local/share/agent-bridge/tasks/` other than your own task id. Per-worker mailboxes are out of scope (relay/scope freeze); this clause is the guard until then.

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

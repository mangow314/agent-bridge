# Vehicle Routing — subagent vs agent-bridge worker

Referenced from ~/.claude/rules/context-discipline.md; SKILL.md pointer to be added by the repo owner.

Once work is spec-complete, pick the vehicle by four axes — any row on the right → agent-bridge worker; all rows left → subagent (executor/verifier/…):

| subagent | agent-bridge worker |
|---|---|
| same vendor suffices | cross-vendor needed (e.g. codex review) |
| lifetime ≤ this main session | residual context must survive main-session /clear or restart |
| no further fan-out | needs third-layer delegation (worker dispatches its own subagents) |
| no human observation | want to watch / intervene in the pane live |

Runtimes: `--runtime codex | claude | agy`. `agy` (Antigravity CLI) is the third
vendor, added for plan-stage second opinions — see `share/review-overlay.md` for when
to run codex and agy side by side, and `docs/agy-probe.md` for its measured limits
(no hooks → notifications always legacy send-keys).

Workers spawned for execution carry an explicit `--model` (see agent-bridge `share/orchestrator-brief.md`); omitting it inherits the CLI's user-default model — acceptable only once you've confirmed that default is the tier you actually want for this worker.

Picking the worker vehicle does **not** itself authorize fan-out: third-layer delegation still requires explicit authorization written into the request (worker-brief contract).

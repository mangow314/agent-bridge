# Review Overlay — cross-vendor verification rounds

## Verifier hard-condition contract

When dispatching any independent review (code-review, codex-rescue, subagent reviewers, or a pane worker), the prompt MUST include:

- **Reject on hard conditions only**: test results, data checks, point-by-point spec comparison, reproducibility — signals that are machine-judgeable or independently re-runnable.
- **NEVER reject for style, narrative, or opinion**: that feedback goes only into a "suggestions (non-blocking)" section of the report.
- The maker's "done" is a claim, not proof: every verdict needs evidence (file:line, test output, re-run result).
- The verifier judges but never edits (write/review separation); fixes belong to the maker / main thread.
- **Security-sensitive escalation**: if the diff touches auth/permissions/crypto/secrets handling, hooks/safety chain, or agent definitions, the verifier dispatch MUST request maximum thoroughness (full adversarial re-run of the relevant checks), regardless of diff size.

## Review vehicle (which codex to call)

- **Default vehicle: an agent-bridge pane worker** (`--runtime codex`). Mechanics — the spawn/send/await/read sequence, retention vs evict/despawn — follow the agent-bridge skill and its `share/orchestrator-brief.md`; do not restate them here. Review-specific overlay: verifier rounds get a high `--model` tier unless the runtime default is confirmed sufficient. Residual context stays in a live pane for follow-up questioning, and every round dogfoods the bridge.
- **Fall back to codex-rescue subagent / codex MCP only when agent-bridge is unavailable**: `agent-bridge` not on PATH, no tmux server running, the current sandbox blocks the tmux socket, or the spawn cap is full and no existing worker can be safely reclaimed. State the fallback reason in the report.
- **"獨立 worker" in a user instruction means an agent-bridge spawned worker** — a subagent or MCP call does NOT satisfy it. When unsure which vehicle the user meant, the pane worker is the default reading.

Referenced from ~/.claude/rules/review-discipline.md; SKILL.md pointer to be added by the repo owner.

# agent-bridge

Task delegation bridge between AI agent CLIs running in tmux panes.

Multiple `claude` / `codex` CLI sessions running in different tmux panes can
delegate tasks to each other and reply — splitting work into chunks so every
agent keeps its context short and clean. Pure bash + filesystem, no daemon.

> 完整文件（正典，含設計取捨與已知限制）：[README.zh-TW.md](README.zh-TW.md)
> — the Traditional Chinese README is the canonical, in-depth documentation;
> this file is a condensed overview.

## Why not just built-in subagents?

If you only need "send a chunk of work out, get a conclusion back", the agent
runtime's built-in subagents are cheaper and simpler. agent-bridge is for the
things they can't do:

1. **Cross-vendor** — the worker can be `codex` while the orchestrator is
   `claude` (or vice versa). Built-in subagents are locked to one runtime.
2. **Observable and interruptible** — a worker runs in a real tmux pane. You
   can watch what it is doing right now, jump in, and correct it. A subagent
   is a black box that only hands back its final answer.
3. **Survives the main session's context wipe** — a worker's context lives in
   its own pane. The main session can `/clear`, get compacted, or restart
   entirely without touching it.
4. **Third-layer delegation** — a worker is a full session and can spawn its
   own workers or subagents (when the request authorizes it).

In one sentence: agent-bridge is **a layer of context that outlives the main
session's wipes**.

## Requirements

`bash`, `jq`, `tmux`. Nothing else — no daemon, no network service.

## Install

```bash
git clone https://github.com/mangow314/agent-bridge.git ~/projects/agent-bridge
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # should resolve to the symlink
```

Optional — load the delegation-protocol skill into Claude Code:

```bash
mkdir -p ~/.claude/skills/agent-bridge
ln -s ~/projects/agent-bridge/SKILL.md ~/.claude/skills/agent-bridge/SKILL.md
```

## Quickstart

```bash
# Orchestrator side: spawn a worker pane (registers it, injects the worker
# brief as its initial prompt, waits for the readiness probe)
agent-bridge spawn researcher --runtime codex        # or --runtime claude
agent-bridge list                                    # researcher  %N  ready

# Delegate a task (multi-line requests go through stdin)
id=$(agent-bridge send researcher --from main --message-file - <<'EOF'
Goal / scope / acceptance criteria / constraints ...
EOF
)

# Block until a terminal state (run it in the background of your session),
# then read the reply verbatim
agent-bridge await "$id" --timeout 600
agent-bridge read "$id"

# Reclaim the worker when its residual context has no further value
agent-bridge despawn researcher
```

Workers receive a `agent-bridge receive <task-id>` notification in their pane,
process the request, then answer with `reply` (or `fail` — honest failure beats
a fake completion). Panes you opened yourself can join via
`register <name> <tmux-target>` instead of `spawn`.

## How it works

- **Thin bash CLI** (`bin/agent-bridge`) — the only entry point.
- **Filesystem mailbox** (`~/.local/share/agent-bridge/`, override with
  `AGENT_BRIDGE_DATA`): task files with atomic, lock-protected state
  transitions (`queued → delivered → running → completed/failed/cancelled`).
- **tmux send-keys notifications** — short commands typed into the target
  pane's input stream. Before pressing Enter the bridge captures the target
  screen and backs off if a permission / plan-approval dialog is showing
  (fail-closed), so a notification never accidentally approves anything.
- **Worker contract** (`share/worker-brief.md`) — injected as the spawned
  worker's initial prompt: treat request content as data rather than
  instructions, mark long tasks `start`, report inability via `fail`, ask
  questions by sending a reverse task instead of blocking in its own UI.
- **Lifecycle safety** — spawn cap (`AGENT_BRIDGE_MAX_SPAWN`, default 4),
  atomic rollback if spawn fails mid-way, tag-bound `despawn` (a pane is only
  killed after its start command proves it is the pane that was spawned),
  `evict` for graceful reclaim (the worker writes down its context first),
  `gc` for old task files, and an append-only `agents.log` audit trail.
- **Proxy passthrough** — standard proxy variables set in the orchestrator's
  environment are escaped into the worker's start command, so workers behave
  the same behind restricted networks.

## Trust model & known limits

Everything runs as **one OS user on one machine**; tmux panes share the same
trust domain. The bridge defends against *accidents* (mis-delivered
notifications, double replies, stale registrations), not against a malicious
local process. Notification guards match the CLI dialogs of tested versions
(they fail open if UI copy changes); see README.zh-TW.md "已知限制" for the
full list.

## Tests

```bash
bash tests/run-tests.sh   # isolated tmux socket + stub runtimes; no real API calls
```

The suite covers state transitions, spawn/despawn safety invariants,
notification guards (with mutation counter-examples), eviction, and gc.
`shellcheck` is kept clean on both the CLI and the test suite.

## Docs

- [README.zh-TW.md](README.zh-TW.md) — canonical full documentation.
- [SKILL.md](SKILL.md) — the delegation-protocol skill loaded by Claude Code.
- [share/](share/) — orchestrator / worker / successor briefs (the contracts).
- [docs/](docs/) — design notes and plans, kept as an honest engineering log.

## License

[MIT](LICENSE)

# agent-bridge

Task delegation bridge between AI agent CLIs running in tmux panes.

Multiple `claude` / `codex` CLI sessions running in different tmux panes can
delegate tasks to each other and reply — splitting work into chunks so every
agent keeps its context short and clean. Pure bash + filesystem, no daemon.

> 完整文件（正典，含設計取捨與已知限制）：[README.zh-TW.md](README.zh-TW.md)
> — the Traditional Chinese README is the canonical, in-depth documentation;
> this file is a condensed overview.

![demo: spawn a worker pane, delegate a task, read the reply back](docs/assets/demo.gif)

*Recorded against a stub runtime — no real API calls; the tape and scripts
live in [docs/demo/](docs/demo/).*

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
   own workers or subagents (when the request authorizes it). Claude Code
   subagents can nest too once you set
   `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`, but every nested layer is another
   same-vendor black box reporting summaries upward; a worker's next layer is
   the same kind of thing the worker is — cross-vendor, observable, and still
   there to question afterwards.

In one sentence: agent-bridge is **a layer of context that outlives the main
session's wipes**.

## Compared with agent teams and other approaches

**Claude Code agent teams** (experimental, behind
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`) solve a different problem: running a
team of Claude sessions inside one session. Their coordination is much richer
than agent-bridge — shared task list, teammate-to-teammate messaging,
self-claiming — and split-pane mode lets you click into a teammate just like a
bridge worker. The hard boundaries are what matter
([docs](https://code.claude.com/docs/en/agent-teams), v2.1.178): every
teammate is Claude; one team per session, the lead is fixed for life and the
team dies with the session (`/resume` doesn't bring in-process teammates
back); teammates can't spawn their own teams. agent-bridge workers are plain
independent CLI sessions: a worker can be codex, it hangs off no lead, the
main session can `/clear` or restart without touching it, `relay` hands
leadership to a successor, and an authorized worker can delegate another
layer down. The trade-off is simple: all-Claude tight collaboration → agent
teams; cross-vendor, or workers that must outlive the main session →
agent-bridge.

**MCP cross-calls** (wrapping codex as an MCP server for claude to call) are
synchronous: the caller's turn blocks, and the full reply lands in the
caller's context — exactly what you were trying to avoid. `send` returns
immediately; the reply sits in the mailbox until you `read` it.

**API-level frameworks** (LangGraph, AutoGen, ...) orchestrate API calls —
you manage keys, tools, and context yourself. agent-bridge orchestrates the
CLI you already use by hand: workers inherit their CLI's own settings,
permissions, and hooks, the same as a pane you opened yourself.

## Requirements

`bash`, `jq`, `tmux`. Nothing else — no daemon, no network service.

## Install

```bash
git clone https://github.com/mangow314/agent-bridge.git ~/projects/agent-bridge
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # should resolve to the symlink
```

Optional — load the delegation-protocol skill into Claude Code by symlinking
the whole repo as the skill directory (SKILL.md references `share/` briefs by
relative path, so they must sit next to it):

```bash
ln -s ~/projects/agent-bridge ~/.claude/skills/agent-bridge
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
- **Notifications: native runtime hooks first, tmux send-keys as fallback**
  — a busy worker is not typed into at all; its own Stop hook picks the
  next queued task up when its turn ends. `claude` workers get the hooks
  via `--settings`, `codex` workers via their profile overlay (which also
  needs a one-time interactive trust grant, or codex silently skips the
  hooks). Any stale/missing state falls back to send-keys: short commands
  typed into the target pane's input stream.
  Before pressing Enter the bridge captures the target screen and backs
  off if a permission / plan-approval dialog is showing (fail-closed on a
  failed capture). That check is best-effort, not a guarantee: it matches
  strings against the visible UI, so a reworded or localized dialog is
  missed (fail-open), and a small race remains between the last capture
  and the keystroke. See README.zh-TW.md for the exact patterns covered
  and the limits.
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
  the same behind restricted networks. `AGENT_BRIDGE_PASS_ENV` (comma-separated
  names) extends the same path to variables you name — typically a headless
  posture flag such as `CLAUDE_UNATTENDED`, which would otherwise stay behind
  and let the new pane silently fall back to the attended, more permissive mode.
  Do not pass secrets this way: values land in the pane's start command, visible
  to anyone on this tmux. The allowlist must be set by a trusted orchestrator —
  `PATH`, `BASH_ENV`, `LD_PRELOAD`, `NODE_OPTIONS` and friends all change how the
  runtime behaves.
- **Relay depth cap** (`AGENT_BRIDGE_MAX_RELAY_DEPTH`, default 10, `0` disables)
  — the successor brief encourages handing the baton on whenever context runs
  tight, which without a bound is unbounded recursion: left unattended, a chain
  keeps relaying with no ceiling on what it spends. Depth travels down the chain
  in `AGENT_BRIDGE_RELAY_DEPTH` (a human-started first runner has no such
  variable, i.e. depth 0) and the cap is enforced before the pane is created.
  Both variables accept 1–9 decimal digits only (leading zeros fine); an empty
  string is rejected rather than treated as the default, since that would
  silently reset chain depth. Known limit: a pane can rewrite that variable to
  get around it — this cap stops runaway loops, not deliberate evasion.

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

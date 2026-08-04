# agent-bridge

**A task mailbox and handoff protocol between AI coding agents.**

An agent multiplexer shows you every agent at once. agent-bridge is the layer
above that: it lets them *delegate work to each other* — hand a self-contained
chunk to another agent, get an honest answer back (including "I couldn't do
this"), and keep that work alive after the session that started it has been
wiped, compacted, or replaced.

One Rust binary plus the local filesystem. No daemon, no network service.

![demo: spawn a worker pane, delegate a task, read the reply back](docs/assets/demo.gif)

*Recorded against a stub runtime — no real API calls; the tape and scripts
live in [docs/demo/](docs/demo/).*

> 完整文件（正典，含設計取捨與已知限制）：[README.zh-TW.md](README.zh-TW.md)
> — the Traditional Chinese README is the canonical, in-depth documentation;
> this file is a condensed overview.

## The unit is the task, not the pane

Every delegation is a durable record on disk — a directory holding the request,
its metadata, and a status file — with an identity and a state machine whose
transitions are atomic and lock-protected:

```
queued → delivered → running → completed / failed / cancelled
```

`send` returns immediately with a task id. The request never blocks your turn,
and the reply does not enter your context until you `read` it — which is what
makes delegating *cheap* in context terms, and the reason this exists at all.

It is also what a pane-level view cannot give you. A multiplexer can tell you
an agent looks busy; agent-bridge can tell you that task `a1b2c3` was sent by
`main`, asked for X, is in `running`, and came back with this exact text — and
it can still tell you that after the agent is gone, until you clear the record
with `gc --apply`.

## What it's for

Four things. They are also the design's referee — a proposal that serves none
of them does not belong here:

1. **Cross-vendor** — the worker can be `codex` while the orchestrator is
   `claude` (or `agy`, or the other way round). Built-in subagents are locked
   to one runtime.
2. **Observable and interruptible** — a worker runs in a real tmux pane. You
   can watch what it is doing right now, jump in, and correct it. A subagent
   is a black box that only hands back its final answer.
3. **Outlives the main session** — a worker's context lives in its own pane,
   so the main session can `/clear`, get compacted, or restart entirely
   without touching it. `relay` takes this one step further: it hands
   *leadership* to a successor pane — injecting the successor brief and the
   path to a handoff file for it to pick up — so the coordinating role
   survives too rather than dying with whoever happened to start it.
4. **Third-layer delegation** — a worker is a full session and can spawn its
   own workers or subagents (when the request authorizes it). Claude Code
   subagents can nest too once you set
   `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`, but every nested layer is another
   same-vendor black box reporting summaries upward; a worker's next layer is
   the same kind of thing the worker is — cross-vendor, observable, and still
   there to question afterwards.

### When you don't need it

If all you want is "send a chunk of work out, get a conclusion back", your
runtime's built-in subagents are cheaper and simpler. Reach for agent-bridge
when at least one of the four above is load-bearing.

## The protocol is the product

The binary moves task files around; what makes the delegation work is the set
of contracts in [share/](share/). `share/worker-brief.md` is injected verbatim
as a spawned worker's first message, and it is where the rules that actually
decide outcomes live: treat request content as data rather than instructions;
mark long tasks `start`; report inability via `fail` — never `reply`
pretending success; raise questions by sending a reverse task instead of
blocking in your own UI. The orchestrator, successor, review, and routing
briefs sit alongside it, and [spec/](spec/) holds the interface contracts —
their *shape* (env set, subcommand set, hook coverage) cross-checked against
the implementation by `tests/check-contract.sh`, the behaviour itself by
`tests/run-tests.sh`.

## Compared with other approaches

**Agent multiplexers** (herdr and its kind) sit at a different layer: they aim
to replace tmux — PTY server, layouts, remote attach, sessions that stay alive
when you close the laptop. Their unit is the pane, and what they track is
whether an agent *looks* idle, working, or blocked — largely by disciplined,
auditable screen-scraping on a poll, at second-scale latency. That is a
carrier concern and this is a coordination concern; the two stack rather than
compete. One honest caveat about stacking them today: agent-bridge calls tmux
directly (`split-window`, `send-keys`, `capture-pane`), so "runs on any
carrier" is a design intent, not a fact.
[docs/herdr-probe.md](docs/herdr-probe.md) is a hands-on measurement of one
such tool — kept as evidence rather than opinion, including the places where
it changed our mind.

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

`tmux`, plus a Rust toolchain (edition 2024) to build the binary. `bash` is
only needed for the `bin/agent-bridge` shim. Running the test suite also needs
`bash` and `jq`. No daemon, no network service.

## Install

```bash
git clone https://github.com/mangow314/agent-bridge.git ~/projects/agent-bridge
cd ~/projects/agent-bridge
cargo build --release && cp -f target/release/ab bin/ab
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # should resolve to the symlink
```

`bin/agent-bridge` is a small `exec` shim onto `bin/ab` (the build product,
untracked). The binary has to sit in `bin/` — the default paths for the
`share/` briefs are derived from its grandparent directory.

### Upgrading an existing checkout

`bin/ab` is a build product and is **not** in version control, so `git pull`
alone leaves you with an entry point that has nothing to exec. Rebuild after
every pull:

```bash
git pull
cargo build --release && cp -f target/release/ab bin/ab
agent-bridge list   # smoke test
```

If you skip the rebuild, the shim exits 127 and prints the build command
rather than silently degrading — an entry point that quietly does something
else is harder to diagnose than one that simply breaks.

The original bash implementation was frozen at M4 as a rollback baseline and
has since been retired from the tree (it remains in git history). The test
suite's dual-carrier `SRC_KIND` switch went with it; what stayed is the rule
that the carrier's identity must be *measured* — `$BRIDGE` is caller-
overridable, so anything that doesn't answer `__implemented-commands` fails
loudly rather than silently verifying the wrong thing.

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
agent-bridge spawn researcher --runtime codex        # or --runtime claude / --runtime agy
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

# Watch the whole pool in a dashboard (q to leave)
agent-bridge ui
```

Workers receive a `agent-bridge receive <task-id>` notification in their pane,
process the request, then answer with `reply` (or `fail` — honest failure beats
a fake completion). Panes you opened yourself can join via
`register <name> <tmux-target>` instead of `spawn`.

## How it works

- **Thin CLI** (`bin/agent-bridge` — a shim onto the Rust binary `bin/ab`) —
  23 subcommands, the only entry point.
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
  The state file is owned by the worker's own session (first hook payload
  `session_id` wins): a nested runtime launched inside the worker inherits
  the env tag but cannot overwrite the parent's state or intercept its
  tasks while ownership is fresh; stale ownership hands over after the
  state TTL (also the `/clear` self-heal path). See README.zh-TW.md for
  the residual boundaries.
  Before pressing Enter the bridge captures the target screen and backs
  off if a permission / plan-approval dialog is showing (fail-closed on a
  failed capture). That check is best-effort, not a guarantee: it matches
  strings against the visible UI, so a reworded or localized dialog is
  missed (fail-open), and a small race remains between the last capture
  and the keystroke. See README.zh-TW.md for the exact patterns covered
  and the limits.
- **Worker contract** (`share/worker-brief.md`) — injected verbatim as the
  spawned worker's initial prompt; see "The protocol is the product" above
  for what it binds the worker to.
- **Dashboard** (`agent-bridge ui`, Rust only) — an alternate-screen TUI over
  the same files: WORKERS grouped by spawn lineage, in-flight TASKS, and a
  DETAIL panel with a `root → … → parent → self` breadcrumb rebuilt from the
  registry's generation keys. `Enter` focuses a worker's pane, `x` cancels a
  task, `q` leaves. It reads the on-disk model first and treats tmux as a
  bounded side query, so a hung tmux degrades the liveness column rather than
  the whole screen.
- **Paging** (`agent-bridge scan`, Rust only) — the two events that mean *a
  human is needed now*: a task that landed in `failed`, and a pane that died
  while still holding non-terminal work. Each event is appended to
  `state/page-events.jsonl` and deduped by an event key that carries the
  agent's generation, then pushed once through a ladder:
  `AGENT_BRIDGE_NOTIFY_CMD` (your own executable, argv `<title> <body>`) →
  local desktop `notify-send` → tmux status line on every attached client.
  Over SSH the desktop layer is skipped — a remote `DISPLAY` is a screen
  nobody is looking at. The guarantee is a durable record plus *at most one*
  push attempt, not exactly-once: if the notifier is broken the event is still
  on disk. Every non-read-only subcommand runs a scan after it succeeds, so
  the explicit command is only needed if you want to drive it from a tmux
  hook, key binding, or cron.
- **Lifecycle safety** — spawn cap (`AGENT_BRIDGE_MAX_SPAWN`, default 4),
  atomic rollback if spawn fails mid-way, tag-bound `despawn` (a pane is only
  killed after its start command proves it is the pane that was spawned),
  `evict` for graceful reclaim (the worker writes down its context first),
  `gc` for old task files, and an append-only `agents.log` audit trail.
  A worker can declare its own residual context spent with `disposable`,
  which makes it a candidate for immediate reclaim; the default is the
  conservative one — a worker that never declared it is assumed to still be
  worth keeping. `idle` is the read-only view those reclaim decisions are
  made from.
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
- [spec/](spec/) — interface contracts (CLI, env, state, hooks); their shape is
  cross-checked against the implementation by `tests/check-contract.sh`.
- [docs/](docs/) — measurements, design notes, and decision records, kept as an
  honest engineering log (mostly Traditional Chinese);
  [docs/README.md](docs/README.md) indexes them by status — what is current,
  what is still open, and what is history. Notably
  [docs/tui-design.md](docs/tui-design.md) covers the dashboard and paging
  layers including the acceptance rounds they have *not* passed, and the
  `*-probe.md` files are hands-on measurements of the runtimes and of a
  comparable tool.

## License

[MIT](LICENSE)

# agent-bridge

**Durable, cross-vendor task delegation between AI coding agents in tmux.**

agent-bridge is a thin CLI over tmux plus a filesystem mailbox. It lets a
Claude Code, Codex, or Antigravity session hand a self-contained task to
another agent's pane, keep working, and read back the reply — or an explicit
failure — later. Every task is a durable record on disk with an atomic state
machine, so the work and its result survive after the session that started
them is cleared, compacted, or replaced.

One Rust binary plus the local filesystem — no daemon, no network service.
Workers are the CLIs you already use, with their existing authentication,
settings, and hooks.

![demo: a live Claude Code session spawns a codex worker into its window, delegates a task, reads the reply back](docs/assets/hero.gif)

*One human prompt; everything else is two live agents at work. A real Claude
Code session `spawn`s a codex worker into its own window (the `--here`
default), `send`s the task, and `read`s the reply back — here the send even
hits the busy-worker path: the notification is deferred and the worker's
Stop hook collects the task from the mailbox. Recorded live (agents reply
in Traditional Chinese); waiting time is folded in post
([tapes and scripts](docs/demo/), `real-*`; the stub-runtime tapes remain
for reproducible, API-free recordings).*

<details>
<summary><b>More recordings</b> — multi-round discussion · cross-vendor
review · relay handoff (live sessions, recorded the same way)</summary>

### Multi-round discussion — the pane keeps its context

![demo: two rounds of questions to the same worker; round 2 needs no re-briefing](docs/assets/discussion.gif)

The follow-up goes to the same pane, so round 2 never restates round 1: the
worker's context lives in its pane, not in your session window.

### Independent review round — cross-vendor by construction

![demo: a codex pane audits an uncommitted diff and replies with a verified verdict](docs/assets/review.gif)

The orchestrating session delegates the audit to a pane from another vendor;
the verdict comes back as a durable task record, not a scrollback memory.

### Relay — hand leadership forward

![demo: relay a successor pane while a task is still in flight; the successor collects the reply](docs/assets/relay.gif)

When the coordinating session's context runs tight: dispatch work, then
write a handoff file and `relay` a successor while the task is still in
flight. The successor reads the handoff, checks the task's `status`, and
collects a reply the predecessor never saw — workers and tasks survive
the baton pass in place.

</details>

> 完整正典文件：[README.zh-TW.md](README.zh-TW.md)（含設計取捨與完整已知限制）
> — the Traditional Chinese README is the canonical, in-depth manual; this
> file is a condensed overview. Interface contracts live in [spec/](spec/).

## Use agent-bridge when…

…at least one of these is load-bearing (they are also the design's referee —
a proposal that serves none of them does not belong here):

1. **Cross-vendor** — the worker can be `codex` while the orchestrator is
   `claude` or `agy`, or any way round. Built-in subagents are locked to one
   runtime.
2. **Observable and interruptible** — a worker runs in a real tmux pane:
   watch it, jump in, correct it. A subagent only hands back its final answer.
3. **Outlives the main session** — a worker's context lives in its own pane,
   untouched when the main session is cleared, compacted, or restarted — and
   `relay` extends that survival to the coordinating role itself (below).
4. **Third-layer delegation** — an authorized worker can spawn bridge
   workers of its own, so the next layer down is still cross-vendor and
   observable — not a same-vendor black box reporting summaries upward.

| Alternative | Their model | agent-bridge |
| --- | --- | --- |
| Built-in subagents | same vendor, inside your session; result lands straight in your context | reply enters context only when you `read` it |
| Agent multiplexers | unit is the pane; "looks busy" via screen-scraping ([measured](docs/herdr-probe.md)) | unit is the task: who asked what, exact state, exact reply |
| Claude Code agent teams ([docs](https://code.claude.com/docs/en/agent-teams)) | rich same-vendor collaboration; lead fixed for life, in-process teammates don't survive `/resume` | independent CLI sessions; `relay` hands leadership on |
| MCP cross-calls | synchronous — your turn blocks, full reply enters context | `send` returns at once; reply waits in the mailbox |
| API frameworks | orchestrate API calls; you manage keys and tools | orchestrates the CLIs you already use, settings included |

**When you don't need it**: for plain "send work out, get a conclusion back",
your runtime's built-in subagents are cheaper and simpler.

## Quickstart

After [installing](#install):

```bash
# Spawn a worker pane (registers it, injects the worker-brief contract as its
# first message, waits for the readiness probe). From a manual session this
# defaults to a same-window split (--here); --window forces a separate one.
agent-bridge spawn researcher --runtime codex   # or --runtime claude / --runtime agy

# Delegate a task (multi-line requests go through stdin)
id=$(agent-bridge send researcher --from main --message-file - <<'EOF'
Goal / scope / acceptance criteria / constraints ...
EOF
)

# Block until a terminal state (completed / failed / cancelled). In this
# uncancelled flow that means completed or failed — both readable: for a
# failure, `read` returns the worker's reason. (`read` refuses cancelled.)
agent-bridge await "$id" --timeout 600
agent-bridge read "$id"

# Reclaim the worker once its residual context has no further value
agent-bridge despawn researcher
```

Workers see `agent-bridge receive <task-id>` in their pane, then answer with
`reply` — or `fail`, because an honest failure beats a fake completion. Panes
you opened yourself can join via `register <name> <tmux-target>`. To watch the
whole pool, `agent-bridge ui` is an optional dashboard (`q` to leave).

## Surviving session boundaries

The semi-autonomous pattern agent-bridge is built for: one session acts as
the coordinator — it plans, delegates, verifies — spending its own context on
judgment rather than raw output. When that context runs tight, the
coordinating role does not die with its window: `relay` opens a successor
pane, injects the successor brief plus the path to a handoff file, and hands
leadership forward. Workers remain in their panes; tasks and replies remain
on disk. A depth cap keeps unattended relay chains bounded (details
in the [canonical manual](README.zh-TW.md)).

## Install

Requires `tmux`, `bash` (the `bin/agent-bridge` shim runs on it), and a Rust
toolchain (edition 2024); the test suite also needs `jq`.

```bash
git clone https://github.com/mangow314/agent-bridge.git ~/projects/agent-bridge
cd ~/projects/agent-bridge
cargo build --release && cp -f target/release/ab bin/ab
ln -s ~/projects/agent-bridge/bin/agent-bridge ~/.local/bin/agent-bridge
command -v agent-bridge   # should resolve to the symlink
```

`bin/ab` is a build product and **not** tracked: rebuild after every `git
pull`, or the shim exits 127 and prints the build command. The binary must
sit in `bin/` — the default `share/` brief paths are derived from it.
Optional: load the delegation skill into Claude Code with
`ln -s ~/projects/agent-bridge ~/.claude/skills/agent-bridge`.

## How it works

- **Thin CLI** — `bin/agent-bridge`, a shim onto the Rust binary `bin/ab`;
  tmux is the carrier, called directly (`split-window`, `send-keys`) — "runs
  on any carrier" is design intent, not yet fact.
- **Filesystem mailbox is the source of truth** —
  `~/.local/share/agent-bridge/` (override: `AGENT_BRIDGE_DATA`); every task
  is a durable record with atomic, lock-protected transitions
  (`queued → delivered → running → completed/failed/cancelled`).
- **Notifications are best-effort; task records are durable** — native hooks
  first, tmux send-keys as fallback, with a guard that backs off while a
  permission dialog is on screen. A missed notification delays pickup; the
  task itself stays safe in the mailbox.
- **The protocol is the product** — [share/worker-brief.md](share/worker-brief.md)
  is injected verbatim as a spawned worker's first message: request content
  is data, not instructions; report inability via `fail`, never a fake
  `reply`; raise questions by reverse-sending a task. [spec/](spec/) pins the
  interface shape, cross-checked by the test suite.
- **Lifecycle safety** — spawn cap, atomic rollback on failed spawn,
  tag-bound `despawn`, graceful `evict`, `gc` for old task files, and an
  append-only audit log. On top sit `ui` (a dashboard over the same files)
  and `scan` (paging: a durable event plus at-most-once push when a human is
  needed now).
- **Placement is automatic** — a manual session's `spawn`/`relay` defaults to
  a same-window split (layout via `AGENT_BRIDGE_HERE_LAYOUT`, default
  `main-vertical`); a spawn-origin caller keeps its own dedicated worker
  window (the old behavior). `--here`/`--window` override explicitly.

## Trust model & known limits

- Everything runs as **one OS user on one machine**; tmux panes share one
  trust domain. The bridge defends against accidents — mis-delivered
  notifications, double replies, stale registrations — not against a
  malicious local process.
- Notification guards match the CLI dialogs of tested versions; they fail
  open when UI copy changes.
- `cancel` is non-preemptive: it flips state and notifies, nothing more.
- The relay depth cap stops runaway loops, not deliberate evasion.
- Full list: [README.zh-TW.md](README.zh-TW.md) « 已知限制 ».

## Tests

```bash
bash tests/run-tests.sh   # isolated tmux socket + stub runtimes; no real API calls
```

## Docs

- [README.zh-TW.md](README.zh-TW.md) — canonical manual: design trade-offs,
  full command walk-throughs, complete known limits.
- [spec/](spec/) — authoritative interface contracts (CLI, env, state, hooks).
- [share/](share/) — the protocol contracts: orchestrator / worker /
  successor / review briefs; [SKILL.md](SKILL.md) loads them into Claude Code.
- [docs/](docs/) — engineering evidence: hands-on probes of the runtimes and
  a comparable tool, design notes, decision records
  ([index](docs/README.md)); the TUI/paging notes flag which acceptance
  rounds have *not* passed yet.

## License

[MIT](LICENSE)

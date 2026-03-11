# Researcher Agent Memory

## Project: ambient-task-agent
- Root: `/Users/tazawa-masayoshi/Documents/personal-dev/ambient-task-agent`
- Primary language: Rust
- Key dirs: `src/`, `config/`, `spec/`, `.claude/`

## External References Investigated
- **OpenFang** (https://github.com/RightNow-AI/openfang): Open-source Agent OS in Rust, 14 crates, 137K LOC.
  Key architectural patterns documented in `openfang-patterns.md`.

## Useful Pattern Sources
- OpenFang kernel: workflow engine, supervisor, approval gates, cron, triggers, event bus
- OpenFang runtime: agent loop (50-iter max), workspace sandbox, tool policy (deny-wins), context budget
- Agent config format: TOML `agent.toml` per agent directory under `agents/`
- **PicoClaw** (https://github.com/sipeed/picoclaw): Ultra-lightweight AI assistant in Go (Feb 2026).
  Key patterns: message bus (3-channel: inbound/outbound/media), agent registry, subagent spawn (async+sync),
  model routing by complexity (rule classifier, 0.35 threshold), context caching (mtime invalidation),
  3-tier filesystem sandbox (host/sandboxed/whitelist), shell sandbox (40+ deny patterns).
  Patterns documented below in `picoclaw-patterns.md`.
- **ZeroClaw** (https://github.com/zeroclaw-labs/zeroclaw): Rust single-binary agent runtime (25.9k stars, Feb 2026).
  Key patterns: Lane Queue per session, worktree isolation rules (one wt per PR/scope), 7-gate security stack,
  9-state lifecycle FSM, graduated emergency stop (KillAll/NetworkKill/DomainBlock/ToolFreeze),
  daemon supervision with exponential backoff, autonomy levels (full/assisted/supervised).
  Concurrency: max_concurrent=24 default, queue_poll_ms=250, queue_wait_ms=30000, load_window_secs=240,
  strategy="least_loaded". Heartbeat=30min interval (HEARTBEAT.md). No DB for task state (in-memory Tokio).
  Patterns documented in `zeroclaw-patterns.md`.
- **OpenClaw** (https://github.com/openclaw/openclaw): TypeScript autonomous agent (247k stars, Mar 2026).
  Key patterns: 4-lane FIFO queue (main/cron/subagent/nested), per-session serialization (cap=1),
  global throttle via agents.defaults.maxConcurrent, sessions_spawn for child agents (heavyweight),
  Lobster for deterministic multi-step pipelines (no DB, environment-based state).
  Heartbeat: HeartbeatRunner every 30min (configurable), reads HEARTBEAT.md, skips if main lane busy.
  Task state: NO plan→DB→execute pattern; pure in-memory queue + session history.
  Concurrency docs: https://docs.openclaw.ai/concepts/queue, https://docs.openclaw.ai/gateway/heartbeat

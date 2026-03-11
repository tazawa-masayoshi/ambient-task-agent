# ZeroClaw Architecture Patterns

Source: https://github.com/zeroclaw-labs/zeroclaw
Investigated: 2026-03-11

## Project Identity

- Rust-based single-binary AI agent runtime ("runtime OS for agentic workflows")
- ~25.9k stars, 1458 commits, active community
- Positioned as ultra-lightweight OpenClaw alternative (99% less RAM, 400x faster startup)
- Focus: deploy anywhere (Raspberry Pi to cloud), swap any component via config

## Cargo Workspace Structure

4 crates:
- `zeroclaw` (root): main binary + runtime subsystems
- `zeroclaw-core`: shared traits and types
- `zeroclaw-types`: serializable data models
- `robot-kit`: hardware peripheral abstraction

Directory discipline: `agent/` (orchestration), `channels/` (transport), `providers/` (model I/O),
`security/` (policy), `tools/` (execution). Single-purpose modules, dependency inward to contracts.

## Core Architecture Pattern: Trait-Driven Pluggability

Every major subsystem = Rust trait with multiple implementations, swappable via config:
- `Provider` trait: LLM backends (22+ providers)
- `Channel` trait: messaging platforms (15+ channels incl. Slack, Discord, Telegram)
- `Tool` trait: agent capabilities (shell, git, browser, HTTP)
- `Memory` trait: persistence backends (SQLite+FTS5+vector, PostgreSQL, Markdown)
- `SecurityPolicy` trait: authorization enforcement
- `Peripheral` trait: hardware abstraction

Factory pattern: `create_provider()` instantiates at runtime from config, zero-code swaps.

## Runtime Modes

3 modes from single binary:
1. `agent` - CLI REPL or single-message execution with tool calling
2. `gateway` - HTTP/WebSocket server (webhooks, API access), port 8080/42617
3. `daemon` - supervisor mode: gateway + channels + scheduler + heartbeat, with independent
   task supervision and exponential backoff restart (2s→60s)

One process per bot, single-threaded async (Tokio work-stealing executor).

## Agent Execution Loop

1. Message ingestion via Channel or HTTP Gateway
2. Context loading from Memory (hybrid vector 70% + FTS5 keyword 30%)
3. Query classification → model routing (QueryClassifier)
4. Provider selection via factory with fallback chains
5. Research phase: multiple tool calls to gather facts (reduces hallucination)
6. Tool execution loop (bounded by `max_tool_iterations`)
7. Security validation at each tool step
8. Memory persistence of conversation history

## Lane Queue / Session Isolation (Gateway)

Gateway implements Lane Queues:
- Each session gets dedicated execution lane with configurable concurrency (default: 1)
- Session Router: creates new lane for fresh session OR enqueues to existing session lane
- Prevents race conditions during multi-step tool chains where message ordering matters

This is the ambient agent's equivalent of "one Claude Code process per task stream."

## Workspace / Worktree Isolation (AGENTS.md)

Explicit rules for autonomous agent development in the repo itself:
- One dedicated git worktree per active branch/PR stream
- Never work directly in shared default workspace
- Each worktree = single branch + single concern (no mixing unrelated edits)
- Worktree naming: `wt/ci-hardening`, `wt/provider-fix` (scope-based)
- Cleanup after PR merge/close: `git worktree prune`, `git fetch --prune`
- Queue safety rule: assign only currently active target, do not pre-assign future queued targets

## Autonomy Levels / Human Review Workflow

3 autonomy levels:
- `full` - no human approval required
- `assisted` - approval needed for high-risk actions
- `supervised` - approval required for all tool executions

Security enforcement (7 gates, defense-in-depth):
1. Autonomy level check
2. Emergency stop state management (KillAll, NetworkKill, DomainBlock, ToolFreeze)
3. One-time password (OTP) validation for sensitive actions
4. Command allowlist with risk classification (high/medium/low-risk tools)
5. Workspace path validation (symlink detection, path canonicalization)
6. Rate limiting per time window
7. Domain validation for browser navigation

Human-in-the-loop: critical tools (deletions, financial, credentials) pause execution until
explicit user confirmation arrives via WebSocket control interface.

## Agent Lifecycle State Machine (Issue #2308, PR #2316)

9 states:
- Created → Starting → Running → Degraded / Suspended / Backoff → Terminating → Terminated / Crashed

Design: "typed state + guarded transition API + synchronization model"
Persistence: in-memory source of truth + persisted snapshot, optional event journal
Integration points: daemon management, health monitoring, status surfaces, channel intake gating

## Memory System

SQLite-based hybrid search:
- 70% vector (cosine similarity, OpenAI text-embedding-3-small or custom)
- 30% FTS5 BM25 keyword
- LRU embedding cache (10k entries)
- Alternative backends: PostgreSQL + pgvector, Markdown filesystem

## Configuration Layering

Priority: CLI flags > environment variables > `config.toml` > defaults
Config path: `~/.zeroclaw/config.toml` or workspace marker file
Secrets: ChaCha20-Poly1305 AEAD encryption
Hot-reloadable: provider, model, API key settings

## Emergency Stop

`estop engage/resume` commands with OTP validation when enabled.
State levels: KillAll, NetworkKill, DomainBlock, ToolFreeze (graduated severity).
Resume requires OTP validation in security config.

## Scheduling / Cron

`cron add/list/remove/update/trigger` - 5-field cron expressions, RFC3339, intervals
Manual trigger support for testing.

## Channels / Communication

`channel start` - supervised listener with exponential backoff restart
`channel doctor` - 10-second health check per channel
Channels are independent supervised tasks (crash-isolated from each other)

## Key Patterns Relevant to ambient-task-agent

1. **Lane Queue per session** = one execution context per task, serialized, ordered
2. **Worktree isolation rules** = one worktree per PR/task, named by scope, cleanup on close
3. **Autonomy level + OTP** = graduated human approval gates, not binary approve/deny
4. **7-gate security stack** = defense-in-depth for tool execution validation
5. **Emergency stop with graduated severity** = KillAll vs NetworkKill vs ToolFreeze
6. **Daemon mode supervision** = crash-isolated tasks with exponential backoff restart
7. **Research phase before action** = multi-tool-call grounding before response generation
8. **Trait-driven channels** = same agent loop, pluggable communication backend (Slack/CLI/etc.)
9. **Hot-reloadable config** = no restart needed for provider/model changes
10. **Health checks per channel** = `doctor` command for diagnosing channel connectivity

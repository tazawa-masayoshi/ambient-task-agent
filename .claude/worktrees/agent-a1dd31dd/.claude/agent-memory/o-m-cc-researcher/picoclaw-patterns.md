# PicoClaw Architecture Patterns

Source: https://github.com/sipeed/picoclaw
Language: Go | Stars: 23K+ | Released: Feb 2026

## Message Bus (pkg/bus)
- 3 separate buffered channels: inbound, outbound, outbound_media (capacity 64 each)
- Atomic bool for closed state; Close() drains buffers but does NOT close channels (prevents panic)
- All pub/sub operations accept context for cancellation/timeout
- Pattern: use separate channels by message type rather than a single muxed channel

## Agent Registry (pkg/agent/registry.go)
- Map of agentID -> AgentInstance; implicit "main" agent if none configured
- RouteResolver resolves incoming message to target agent
- CanSpawnSubagent() enforces allowlist (supports "*" wildcard) before delegation
- ForEachTool() propagates shared tools to all agents after registration

## Agent Loop (pkg/agent/loop.go)
- Main loop: consume inbound -> route -> load history -> LLM iteration -> publish outbound
- Tool calls executed in parallel (goroutines + WaitGroup)
- Retry with exponential backoff on timeout
- Context window exceeded -> compression pass
- Session summarization at configurable token threshold (auto-truncate after)
- Model routing: light model vs primary model based on complexity score

## Tool Loop (pkg/tools/toolloop.go)
- Reusable RunToolLoop(): same logic used by main agents AND subagents
- Max iterations configurable (default 10 for subagents)
- Loop termination: no tool calls in LLM response = done
- Results feed back as "tool" role messages for next iteration

## Subagent Spawn (pkg/tools/subagent.go + spawn.go)
- SubagentManager: task registry (map[id]Task), RWMutex
- Spawn(): async, returns immediately with task ID; callback on completion
- SubagentTool: sync wrapper that blocks until done
- Tasks track status: running -> completed/failed/canceled
- SpawnTool validates against allowlist before delegation
- Async result pattern: caller receives confirmation, result arrives later via callback

## Model Routing (pkg/routing)
- RuleClassifier: 6 signals, O(1) decision, sub-microsecond
  - Token length: >200 = +0.35, 50-200 = +0.15
  - Code block: +0.40
  - Recent tool calls: >3 = +0.25, 1-3 = +0.10
  - Conversation depth: >10 turns = +0.10
  - Attachments: hard gate = 1.0
  - Cap at 1.0
- Default threshold: 0.35
- Returns (model_name, is_light, score)

## Context Caching (pkg/agent/context.go)
- ContextBuilder caches static system prompt: identity + bootstrap files + skills + memory
- mtime check on source files -> auto-invalidates cache
- Dynamic parts appended per-request: time, session summary
- SystemParts struct: enables provider-specific optimizations (Anthropic ephemeral cache)
- Read-write lock: fast-path read, escalate to write only on cache miss

## Filesystem Sandbox (pkg/tools/filesystem.go)
- 3-tier architecture:
  1. hostFs: unrestricted os package access
  2. sandboxFs: os.Root confinement, relative paths only, rejects "../" escapes
  3. whitelistFs: wraps sandboxFs, regex-based exceptions for specific external paths
- Atomic writes: temp file + Sync() + rename, permissions 0o600
- Symlink validation: resolve and check against workspace boundary

## Shell Sandbox (pkg/tools/shell.go)
- Platform-specific: `sh -c` on Unix, PowerShell on Windows
- 40+ regex deny patterns: rm -rf, sudo, chmod, eval, backtick, $() substitution, docker, pkg managers
- Workspace restriction: validates absolute paths against working directory
- Allow-list override: custom patterns exempt specific trusted commands
- Resource: 60s timeout, 10K char output truncation
- Process termination: graceful shutdown -> forced kill

## Session/Memory Persistence (pkg/session, pkg/memory)
- JSONL backend for session history (migrated from JSON)
- SessionManager creates per-agent session stores
- Memory: store.go + JSONL persistence + migration support
- Automatic legacy migration on startup

## MCP Integration (pkg/mcp/manager.go)
- Auto-detects transport: stdio for local, SSE/HTTP for remote
- Loads env vars from .env files per server config
- Concurrent server init with error aggregation
- Atomic closed flag + WaitGroup for graceful shutdown
- Double-check locking pattern to prevent TOCTOU races on tool calls

## Scheduled Tasks (pkg/cron, pkg/tools/cron.go)
- 3 schedule types: one-time (at_seconds), recurring (every_seconds), cron expression
- Jobs store channel+chatID context for routing
- Execution: shell commands -> ExecTool -> publish to bus; agent tasks -> route through executor
- CronTool exposed as LLM tool (add/list/remove/enable/disable)

## Key Architectural Insights for ambient-task-agent
1. Separate inbound/outbound bus channels prevents head-of-line blocking
2. Async subagent spawn + callback is better than blocking for long-running tasks
3. Rule-based model routing (no ML) is sufficient and fast for cost optimization
4. Context cache invalidation by mtime is elegant for file-based config
5. 3-tier filesystem sandbox (host/sandbox/whitelist) maps well to worktree isolation
6. Shell deny-list approach is pragmatic; combine with workspace restriction
7. JSONL for session persistence is append-friendly on constrained hardware

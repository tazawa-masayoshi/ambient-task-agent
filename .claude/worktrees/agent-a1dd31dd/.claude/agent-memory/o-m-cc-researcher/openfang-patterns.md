# OpenFang Architecture Patterns

Source: https://github.com/RightNow-AI/openfang (13.6k stars, Rust, 137K LOC)

## Core Architecture

14 crates: kernel, runtime, api, channels, memory, hands, skills, extensions, types, wire, cli, desktop, migrate, hands

## Key Patterns for Ambient Task Agents

### 1. Workflow Engine (workflow.rs)
- Steps: Sequential | FanOut (parallel, tokio::join_all) | Collect | Conditional | Loop
- Error modes: Fail | Skip | Retry(N)
- Output chaining: previous step output -> next step input
- Template variables: `{{var_name}}` interpolation
- Run retention: FIFO eviction at 200 runs

### 2. Supervisor Pattern (supervisor.rs)
- Tokio watch channel for coordinated shutdown
- Per-agent restart counts with configurable max_restarts
- Panic capture + count (observability)
- Does NOT spawn agents directly—provides signals only

### 3. Approval Gates (approval.rs)
- oneshot channel: agent suspends until human resolves
- States: Approved | Denied | TimedOut (default 60s)
- Per-agent limit: max 5 pending requests
- Risk tiers: Critical (shell_exec) > High (file ops) > Medium (web) > Low
- Hot-reload policy: change which tools need approval without restart

### 4. Background Execution Modes (background.rs)
- Continuous: self-prompt at fixed interval, checks goals + shared memory
- Periodic: simplified cron (e.g. "every 5m")
- Proactive: event-triggered via TriggerEngine
- Global semaphore: max 5 concurrent background LLM calls
- Busy flag: skips tick if previous still running (prevents concurrent self-execution)

### 5. Trigger System (triggers.rs)
- Types: Lifecycle | AgentSpawned | AgentTerminated | System | SystemKeyword | MemoryUpdate | MemoryKeyPattern | ContentMatch | All
- DashMap for thread-safe concurrent access
- evaluate(): checks enabled + max_fires, pattern matches, template-fills message
- Decoupled from scheduler—produces (agent_id, message) pairs

### 6. Event Bus (event_bus.rs)
- tokio broadcast channels: one global + per-agent dedicated channels
- Routing targets: Agent-specific | Broadcast | Pattern-match | System
- Ring buffer history: max 1000 events, FIFO eviction

### 7. Agent Loop (agent_loop.rs)
- Max 50 iterations, 5 continuation limit (token overflow)
- States: Thinking -> ToolUse -> Streaming -> Done/Error
- Tool timeout: 120s per tool
- Auto-trim at 20 messages in history
- Retry: exponential backoff, 3 retries for rate limits
- Circuit breaker: repeated pattern detection
- Error injection: "Report error honestly, do NOT fabricate results"

### 8. Tool Policy (tool_policy.rs)
- Deny-wins: any deny rule overrides allows
- Glob patterns: `shell_*`, `@web_tools` group references
- Depth-aware: strips cron_create/process_start from subagents
- Subagent nesting depth limit enforced

### 9. Workspace Sandbox (workspace_sandbox.rs)
- Path containment via canonicalization (not chroot/container)
- Rejects `..` components outright
- Symlink escape detection via canonical path comparison
- External access delegated to MCP filesystem tools

### 10. Context Budget (context_budget.rs)
- Per-result cap: 30% of context window
- Single result max: 50%
- Total tool headroom: 75%
- Two-layer defense: truncate individual -> compact older results
- UTF-8-safe truncation (walks backward to valid boundary)

### 11. Cron System (cron.rs)
- Schedule types: At (one-shot) | Every (interval) | Cron (5/6-field expression)
- Pre-advance next_run to prevent duplicate firing
- Auto-disable after 5 consecutive failures
- Per-agent limit: 50 jobs; global cap exists
- Cleanup on agent deletion

### 12. Registry (registry.rs)
- Indexes: by ID, by name, by tags
- Stored per agent: model config, cost limits, token quotas, tool permissions, parent/child hierarchy, workspace path, session ID
- Name uniqueness enforced at registration

### 13. A2A Protocol (a2a.rs)
- Google's cross-framework standard, JSON-RPC 2.0
- Agent Cards at `/.well-known/agent.json`
- Task states: Submitted -> Working -> Completed/Failed/Cancelled
- Bounded in-memory store with FIFO eviction

### 14. Agent Config Format
- Per-agent directory: `agents/<name>/agent.toml`
- Fields: model, temperature, max_tokens, tools[], capabilities{}, resource limits, background mode
- Coder agent tools: file_read, file_write, file_list, shell_exec, web_search, web_fetch, memory_store, memory_recall
- Memory isolation: read=`*`, write=`self.*`

## Innovative/Notable Approaches

1. **Pre-advance scheduling**: next_run updated before execution to prevent double-fire under load
2. **Deny-wins + group references**: `@group_name` syntax for reusable tool sets
3. **Busy flag on background ticks**: skip rather than queue concurrent self-execution
4. **Approval as oneshot channel**: clean async suspension, no polling
5. **UTF-8-safe dynamic truncation**: adapts to model context window size
6. **Hot-reload approval policy**: change gates without restart
7. **Decoupled trigger evaluation**: produces messages, doesn't route—caller decides delivery

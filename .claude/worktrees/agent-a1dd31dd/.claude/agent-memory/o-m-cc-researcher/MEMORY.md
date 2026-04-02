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
- **autoresearch** (https://github.com/karpathy/autoresearch): Python 630-line autonomous ML experiment loop (Mar 2026).
  Key patterns: git-ratchet (keep commit if metric improves, `git reset HEAD~1` if not), immutable/mutable
  file separation (constants+prepare.py fixed, train.py mutable), program.md as agent instruction spec,
  "NEVER STOP" directive for unattended loop, TSV logging of all experiments (kept + discarded),
  status=crash for OOM/fundamental errors. No parallelism (single-GPU sequential). ~12 exp/hour.
  Related: Ralph Wiggum loop (single agent in bash loop), Gas Town (30 parallel agents with role hierarchy).
- **multi-agent-shogun** (https://github.com/yohey-w/multi-agent-shogun): tmux+YAML-based feudal multi-agent system (1.1k stars, Mar 2026).
  Key patterns: bottom-up skill discovery (Ashigaru proposes skill_candidate in report YAML → Karo aggregates in dashboard.md → Shogun approves → saved to .claude/commands/),
  skill_candidate YAML fields: {found, name, description, reason}, trigger: repeated 2+ times or cross-project reuse,
  Stop hook in .claude/settings.json (bash scripts/stop_hook_inbox.sh, timeout 60s) for inbox check at turn-end,
  inotifywait-based event-driven mailbox (flock+YAML in queue/inbox/), Bloom taxonomy task routing (L1-L3: Ashigaru, L4-L6: Gunshi),
  skills stored in .claude/commands/ and invoked via /skill-name.
  Applicability: skill_candidate pattern directly applicable to ambient-task-agent self_improvement job.
- **lossless-claw** (https://github.com/Martian-Engineering/lossless-claw): OpenClaw plugin for lossless context management (TS+Go, Voltropy LCM research, Mar 2026).
  Problem: sliding-window truncation loses old messages. Solution: DAG-based hierarchical summarization.
  Key patterns: 5-layer architecture (Persist→Summarize→DAG-Compact→Assemble→Retrieve),
  freshTailCount=32 (protect recent msgs), contextThreshold=0.75 (trigger at 75% window),
  lcm_grep/lcm_describe/lcm_expand tools (agent self-retrieval), 3-tier expansion routing
  (direct-answer/shallow-expand/subagent-delegate based on token_risk>70% or broad+multi-hop),
  expansion-auth (token_cap per grant, TTL, delegated child grants), transcript-repair (tool call/result pair repair),
  integrity checker (8 invariants on DAG contiguity+lineage+no-orphans).
  SQLite with FTS5 for full-text search. summaryModel override (use cheaper model for summarization).
  Applicability to ambient-task-agent: LOW priority now (ops threads are short); revisit if threads reach 100+ turns.
  Most relevant pattern: agent self-retrieval design (lcm_grep) — currently Rust side always prepares context for LLM.

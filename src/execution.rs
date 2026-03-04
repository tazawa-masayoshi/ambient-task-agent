use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

const REGISTRY_CAPACITY: usize = 200;

// ============================================================================
// ExecutionRegistry — 実行履歴リングバッファ
// ============================================================================

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExecutionRecord {
    pub module: String,
    pub timestamp: DateTime<Utc>,
    pub success: bool,
    pub duration_ms: u64,
    pub error_summary: Option<String>,
}

pub struct ExecutionRegistry {
    records: Mutex<VecDeque<ExecutionRecord>>,
}

impl ExecutionRegistry {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(VecDeque::with_capacity(REGISTRY_CAPACITY)),
        }
    }

    pub fn push(&self, record: ExecutionRecord) {
        let mut records = self.records.lock().unwrap();
        if records.len() >= REGISTRY_CAPACITY {
            records.pop_front();
        }
        records.push_back(record);
    }

    /// 指定モジュールの直近 window_secs 秒間の実行回数を返す
    pub fn count_recent(&self, module: &str, window_secs: u64) -> usize {
        let records = self.records.lock().unwrap();
        let cutoff = Utc::now() - chrono::Duration::seconds(window_secs as i64);
        records
            .iter()
            .filter(|r| r.module == module && r.timestamp > cutoff)
            .count()
    }

    #[allow(dead_code)]
    pub fn recent_snapshot(&self, n: usize) -> Vec<ExecutionRecord> {
        let records = self.records.lock().unwrap();
        records.iter().rev().take(n).cloned().collect()
    }
}

// ============================================================================
// HookDecision + ExecutionHook trait
// ============================================================================

pub enum HookDecision {
    Continue,
    Block(String),
}

/// 実行前後に呼ばれるフック（同期 trait: async は dyn 非対応のため）
pub trait ExecutionHook: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &str;
    fn before_run(&self, module: &str, prompt_summary: &str) -> HookDecision;
    fn after_run(&self, record: &ExecutionRecord);
}

// ============================================================================
// HookRegistry
// ============================================================================

pub struct HookRegistry {
    hooks: Vec<Box<dyn ExecutionHook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn register(&mut self, hook: impl ExecutionHook + 'static) {
        self.hooks.push(Box::new(hook));
    }

    /// 全フックの before_run を実行。いずれかが Block を返したら即座に Block を返す
    pub fn run_before(&self, module: &str, prompt_summary: &str) -> HookDecision {
        for hook in &self.hooks {
            match hook.before_run(module, prompt_summary) {
                HookDecision::Continue => {}
                block @ HookDecision::Block(_) => return block,
            }
        }
        HookDecision::Continue
    }

    /// 全フックの after_run を実行
    pub fn run_after(&self, record: &ExecutionRecord) {
        for hook in &self.hooks {
            hook.after_run(record);
        }
    }
}

// ============================================================================
// LoopDetectionHook
// ============================================================================

const LOOP_WARN_THRESHOLD: usize = 5;
const LOOP_CRITICAL_THRESHOLD: usize = 10;
const LOOP_WINDOW_SECS: u64 = 60;

pub struct LoopDetectionHook {
    registry: Arc<ExecutionRegistry>,
}

impl LoopDetectionHook {
    pub fn new(registry: Arc<ExecutionRegistry>) -> Self {
        Self { registry }
    }
}

impl ExecutionHook for LoopDetectionHook {
    fn name(&self) -> &str {
        "loop_detection"
    }

    fn before_run(&self, module: &str, _prompt_summary: &str) -> HookDecision {
        let count = self.registry.count_recent(module, LOOP_WINDOW_SECS);
        if count >= LOOP_CRITICAL_THRESHOLD {
            HookDecision::Block(format!(
                "Loop detected: module '{}' executed {} times in {}s (critical threshold: {})",
                module, count, LOOP_WINDOW_SECS, LOOP_CRITICAL_THRESHOLD
            ))
        } else if count >= LOOP_WARN_THRESHOLD {
            tracing::warn!(
                "Loop warning: module '{}' executed {} times in {}s (warn threshold: {})",
                module,
                count,
                LOOP_WINDOW_SECS,
                LOOP_WARN_THRESHOLD
            );
            HookDecision::Continue
        } else {
            HookDecision::Continue
        }
    }

    fn after_run(&self, record: &ExecutionRecord) {
        self.registry.push(record.clone());
    }
}

// ============================================================================
// ToolResult
// ============================================================================

#[derive(Debug)]
#[allow(dead_code)]
pub enum ToolResult {
    Success(String),
    SoftError(String),
    HardError(anyhow::Error),
}

#[allow(dead_code)]
impl ToolResult {
    pub fn is_success(&self) -> bool {
        matches!(self, ToolResult::Success(_))
    }

    pub fn output(&self) -> &str {
        match self {
            ToolResult::Success(s) => s,
            ToolResult::SoftError(s) => s,
            ToolResult::HardError(e) => {
                // anyhow::Error は &str を直接返せないので空文字列
                // 呼び出し側は HardError をマッチして e.to_string() を使うこと
                let _ = e;
                ""
            }
        }
    }
}

// ============================================================================
// RunnerContext — ClaudeRunner に一括注入する実行コンテキスト
// ============================================================================

#[derive(Clone)]
pub struct RunnerContext {
    pub defaults: crate::repo_config::Defaults,
    pub semaphore: Arc<tokio::sync::Semaphore>,
    /// API 拡張用（recent_snapshot 等）。現在はフック経由でのみ使用
    #[allow(dead_code)]
    pub registry: Arc<ExecutionRegistry>,
    pub hooks: Arc<HookRegistry>,
    /// 起動時に一度解決した環境変数（毎回 std::env::var を呼ばない）
    pub resolved_env: Vec<(String, String)>,
}


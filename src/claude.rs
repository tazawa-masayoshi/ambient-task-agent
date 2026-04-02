use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::anthropic::mcp::McpServerConfig;
use crate::execution::{ExecutionRecord, HookDecision, RunnerContext};
use crate::repo_config::ExecMode;

const MAX_LOG_FILES: usize = 100;

// ============================================================================
// AgentBackend trait — LLM 実行バックエンドの抽象
// ============================================================================

/// LLM バックエンドに渡すリクエスト
/// ストリーミング進捗コールバック（tool_use イベント等を通知）
pub type ProgressCallback = Arc<dyn Fn(ProgressEvent) + Send + Sync>;

/// ストリーミング進捗イベント
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ProgressEvent {
    /// ツール使用中（ツール名）
    ToolUse(String),
}

#[allow(dead_code)] // CLI 廃止後の一部フィールドは MCP 移行で再利用予定
pub struct AgentRequest {
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    pub allowed_tools: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub timeout_secs: Option<u64>,
    pub max_output_bytes: Option<usize>,
    pub resume_session_id: Option<String>,
    pub json_schema: Option<String>,
    pub fallback_model: Option<String>,
    /// ストリーミング進捗コールバック（設定すると stream-json モードで実行）
    pub progress: Option<ProgressCallback>,
    /// MCP サーバー設定（AnthropicApiBackend でのみ使用）
    #[allow(dead_code)]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// LLM バックエンドから返るレスポンス
#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration: std::time::Duration,
    pub truncated: bool,
    /// トークン使用量（JSON出力モードで取得可能）
    pub usage: Option<TokenUsage>,
    /// セッション費用（USD）
    pub cost_usd: Option<f64>,
    /// セッションID（--resume で継続実行に使用）
    pub session_id: Option<String>,
}

/// Claude CLI の JSON 出力から取得できるトークン使用量
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    #[allow(dead_code)]
    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}

impl AgentOutput {
    /// エラー出力を返す（stderr が空なら stdout をフォールバック）
    pub fn error_output(&self) -> &str {
        if self.stderr.is_empty() {
            &self.stdout
        } else {
            &self.stderr
        }
    }

}

/// LLM 実行バックエンドの抽象インターフェース
#[async_trait]
pub trait AgentBackend: Send + Sync {
    async fn execute(&self, request: AgentRequest) -> Result<AgentOutput>;
}

// ClaudeCliBackend は廃止（ADR-0009）。OAuth → API Key → Bedrock のフォールバックチェーンに移行。


// ============================================================================
// ClaudeRunner — ビルダー + 実行制御オーケストレーター
// ============================================================================

#[derive(Debug, Serialize)]
struct ExecutionLog {
    timestamp: String,
    module: String,
    prompt_summary: String,
    system_prompt_summary: Option<String>,
    max_turns: u32,
    allowed_tools: Option<String>,
    cwd: Option<String>,
    success: bool,
    duration_secs: f64,
    output_length: usize,
    output: String,
    error: Option<String>,
    timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    truncated: bool,
    usage: Option<TokenUsage>,
    cost_usd: Option<f64>,
}

pub struct ClaudeRunner {
    module: String,
    prompt: String,
    system_prompt: Option<String>,
    max_turns: u32,
    allowed_tools: Option<String>,
    cwd: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    exec_mode: ExecMode,
    semaphore: Option<Arc<Semaphore>>,
    resolved_env: Option<Vec<(String, String)>>,
    non_blocking: bool,
    hooks: Option<Arc<crate::execution::HookRegistry>>,
    backend: Option<Arc<dyn AgentBackend>>,
    /// セッション継続用: 前回の session_id
    resume_session_id: Option<String>,
    /// JSON Schema（構造化出力モード）
    json_schema: Option<String>,
    /// フォールバックモデル（Opus 過負荷時に自動切替）
    fallback_model: Option<String>,
    /// ストリーミング進捗コールバック
    progress: Option<ProgressCallback>,
    /// MCP サーバー設定
    mcp_servers: Vec<McpServerConfig>,
}

impl ClaudeRunner {
    pub fn new(module: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            module: module.into(),
            prompt: prompt.into(),
            system_prompt: None,
            max_turns: 3,
            allowed_tools: None,
            cwd: None,
            log_dir: None,
            timeout_secs: None,
            max_output_bytes: None,
            exec_mode: ExecMode::Normal,
            semaphore: None,
            resolved_env: None,
            non_blocking: false,
            hooks: None,
            backend: None,
            resume_session_id: None,
            json_schema: None,
            fallback_model: None,
            progress: None,
            mcp_servers: Vec::new(),
        }
    }

    /// ストリーミング進捗コールバックを設定（stream-json モードで実行）
    pub fn on_progress(mut self, cb: ProgressCallback) -> Self {
        self.progress = Some(cb);
        self
    }

    /// JSON Schema を指定して構造化出力モードを有効化
    pub fn json_schema(mut self, schema: impl Into<String>) -> Self {
        self.json_schema = Some(schema.into());
        self
    }

    /// セッション継続: 前回の session_id を指定して --resume で再開
    pub fn resume(mut self, session_id: impl Into<String>) -> Self {
        self.resume_session_id = Some(session_id.into());
        self
    }

    pub fn system_prompt(mut self, sp: impl Into<String>) -> Self {
        self.system_prompt = Some(sp.into());
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = turns;
        self
    }

    pub fn allowed_tools(mut self, tools: impl Into<String>) -> Self {
        self.allowed_tools = Some(tools.into());
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn log_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.log_dir = Some(dir.into());
        self
    }

    pub fn optional_log_dir(self, dir: Option<&Path>) -> Self {
        if let Some(d) = dir {
            self.log_dir(d)
        } else {
            self
        }
    }

    /// interactive 用: semaphore が取得できなければ即エラーにする
    pub fn non_blocking(mut self) -> Self {
        self.non_blocking = true;
        self
    }

    /// RunnerContext から防御設定+フック+バックエンドを一括注入
    pub fn with_context(mut self, ctx: &RunnerContext) -> Self {
        let (exec_mode, timeout) = ctx.defaults.resolve_for_module(&self.module);
        if self.timeout_secs.is_none() {
            self.timeout_secs = Some(timeout);
        }
        if self.max_output_bytes.is_none() {
            self.max_output_bytes = Some(ctx.defaults.claude_max_output_bytes);
        }
        if self.exec_mode == ExecMode::Normal {
            self.exec_mode = exec_mode;
        }
        if self.resolved_env.is_none() {
            self.resolved_env = Some(ctx.resolved_env.clone());
        }
        if self.semaphore.is_none() {
            self.semaphore = Some(ctx.semaphore.clone());
        }
        if self.fallback_model.is_none() {
            self.fallback_model = ctx.defaults.claude_fallback_model.clone();
        }
        self.hooks = Some(ctx.hooks.clone());
        self.backend = Some(ctx.backend.clone());
        self
    }

    #[allow(dead_code)]
    pub fn mcp_servers(mut self, servers: Vec<McpServerConfig>) -> Self {
        self.mcp_servers = servers;
        self
    }

    pub async fn run(self) -> Result<AgentOutput> {
        // 0. Hook: before_run
        if let Some(ref hooks) = self.hooks {
            let prompt_summary = truncate_str(&self.prompt, 200);
            match hooks.run_before(&self.module, prompt_summary) {
                HookDecision::Continue => {}
                HookDecision::Block(reason) => {
                    anyhow::bail!(
                        "ClaudeRunner [{}]: blocked by hook: {}",
                        self.module,
                        reason
                    );
                }
            }
        }

        // 1. ExecMode チェック
        match self.exec_mode {
            ExecMode::Deny => {
                anyhow::bail!(
                    "ClaudeRunner [{}]: execution denied by exec_mode=deny",
                    self.module
                );
            }
            ExecMode::DryRun => {
                tracing::info!("ClaudeRunner [{}]: dry_run mode, skipping execution", self.module);
                return Ok(AgentOutput {
                    success: true,
                    stdout: "[dry_run]".to_string(),
                    stderr: String::new(),
                    duration: std::time::Duration::ZERO,
                    truncated: false,
                    usage: None,
                    cost_usd: None,
                    session_id: None,
                });
            }
            ExecMode::Normal => {}
        }

        // 2. Semaphore acquire
        let _permit = match &self.semaphore {
            Some(sem) if self.non_blocking => Some(
                sem.try_acquire()
                    .map_err(|_| anyhow::anyhow!(
                        "ClaudeRunner [{}]: all execution slots are busy (non_blocking mode)",
                        self.module
                    ))?,
            ),
            Some(sem) => Some(
                sem.acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("Semaphore closed: {}", e))?,
            ),
            None => None,
        };

        // 3. AgentRequest 構築 → バックエンド実行
        tracing::info!(
            "ClaudeRunner [{}]: max_turns={}, system_prompt={}, cwd={:?}, timeout={:?}s",
            self.module,
            self.max_turns,
            self.system_prompt.is_some(),
            self.cwd.as_ref().map(|p| p.display().to_string()),
            self.timeout_secs,
        );

        let request = AgentRequest {
            prompt: self.prompt.clone(),
            system_prompt: self.system_prompt.clone(),
            max_turns: self.max_turns,
            allowed_tools: self.allowed_tools.clone(),
            cwd: self.cwd.clone(),
            env: self.resolved_env.clone().unwrap_or_default(),
            timeout_secs: self.timeout_secs,
            max_output_bytes: self.max_output_bytes,
            resume_session_id: self.resume_session_id.clone(),
            json_schema: self.json_schema.clone(),
            fallback_model: self.fallback_model.clone(),
            progress: self.progress.clone(),
            mcp_servers: self.mcp_servers.clone(),
        };

        let backend = self.backend.as_ref()
            .map(Arc::clone)
            .expect("AgentBackend must be set before ClaudeRunner::run()");

        let result = backend.execute(request).await?;

        if !result.success {
            tracing::warn!(
                "ClaudeRunner [{}]: failed: {}",
                self.module,
                result.stderr
            );
        }

        // 4. Hook: after_run
        if let Some(ref hooks) = self.hooks {
            let record = ExecutionRecord {
                module: self.module.clone(),
                timestamp: chrono::Utc::now(),
                success: result.success,
                duration_ms: result.duration.as_millis() as u64,
                error_summary: if result.success {
                    None
                } else {
                    Some(truncate_str(result.error_output(), 200).to_string())
                },
            };
            hooks.run_after(&record);
        }

        // 5. 非同期ログ書き込み
        if let Some(log_dir) = self.log_dir {
            let log = ExecutionLog {
                timestamp: chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
                module: self.module.clone(),
                prompt_summary: truncate_str(&self.prompt, 200).to_string(),
                system_prompt_summary: self
                    .system_prompt
                    .as_deref()
                    .map(|sp| truncate_str(sp, 200).to_string()),
                max_turns: self.max_turns,
                allowed_tools: self.allowed_tools.clone(),
                cwd: self.cwd.as_ref().map(|p| p.display().to_string()),
                success: result.success,
                duration_secs: result.duration.as_secs_f64(),
                output_length: result.stdout.len(),
                output: result.stdout.clone(),
                error: if result.stderr.is_empty() {
                    None
                } else {
                    Some(result.stderr.clone())
                },
                timeout_secs: self.timeout_secs,
                max_output_bytes: self.max_output_bytes,
                truncated: result.truncated,
                usage: result.usage.clone(),
                cost_usd: result.cost_usd,
            };
            let module = self.module.clone();
            tokio::spawn(async move {
                if let Err(e) = write_log(&log_dir, &log).await {
                    tracing::warn!("Failed to write execution log for {}: {}", module, e);
                }
            });
        }

        Ok(result)
    }
}

pub(crate) fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

async fn write_log(log_dir: &Path, log: &ExecutionLog) -> Result<()> {
    tokio::fs::create_dir_all(log_dir).await?;

    let filename = format!(
        "{}_{}.json",
        log.timestamp.replace(':', "-"),
        log.module
    );
    let path = log_dir.join(&filename);

    let json = serde_json::to_string_pretty(log)?;
    tokio::fs::write(&path, json).await?;

    rotate_logs(log_dir, MAX_LOG_FILES).await;

    Ok(())
}

async fn rotate_logs(log_dir: &Path, max_files: usize) {
    let mut entries = match tokio::fs::read_dir(log_dir).await {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut files: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            files.push(path);
        }
    }

    if files.len() <= max_files {
        return;
    }

    files.sort();
    let to_remove = files.len() - max_files;
    for path in files.into_iter().take(to_remove) {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!("Failed to remove old log file {}: {}", path.display(), e);
        }
    }
}

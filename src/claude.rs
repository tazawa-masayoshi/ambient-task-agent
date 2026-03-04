use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::execution::{ExecutionRecord, HookDecision, RunnerContext, ToolResult};
use crate::repo_config::ExecMode;

const MAX_LOG_FILES: usize = 100;

#[derive(Debug, Clone)]
pub struct ClaudeResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration: std::time::Duration,
}

impl ClaudeResult {
    /// エラー出力を返す（stderr が空なら stdout をフォールバック）
    pub fn error_output(&self) -> &str {
        if self.stderr.is_empty() {
            &self.stdout
        } else {
            &self.stderr
        }
    }

    #[allow(dead_code)]
    pub fn into_tool_result(self) -> ToolResult {
        if self.success {
            ToolResult::Success(self.stdout)
        } else {
            ToolResult::SoftError(self.error_output().to_string())
        }
    }
}

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
    /// 事前解決済みの環境変数 (key, value) ペア
    resolved_env: Option<Vec<(String, String)>>,
    /// true の場合、semaphore が取得できなければ即エラー（interactive 用）
    non_blocking: bool,
    hooks: Option<Arc<crate::execution::HookRegistry>>,
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
        }
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

    #[allow(dead_code)]
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    #[allow(dead_code)]
    pub fn max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = Some(bytes);
        self
    }

    #[allow(dead_code)]
    pub fn exec_mode(mut self, mode: ExecMode) -> Self {
        self.exec_mode = mode;
        self
    }

    #[allow(dead_code)]
    pub fn semaphore(mut self, sem: Arc<Semaphore>) -> Self {
        self.semaphore = Some(sem);
        self
    }

    #[allow(dead_code)]
    pub fn allowed_env(mut self, keys: Vec<String>) -> Self {
        self.resolved_env = Some(resolve_env(&keys));
        self
    }

    /// interactive 用: semaphore が取得できなければ即エラーにする
    #[allow(dead_code)]
    pub fn non_blocking(mut self) -> Self {
        self.non_blocking = true;
        self
    }

    /// RunnerContext から防御設定+フックを一括注入（既に設定済みの値は上書きしない）
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
        self.hooks = Some(ctx.hooks.clone());
        self
    }

    pub async fn run(self) -> Result<ClaudeResult> {
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
                return Ok(ClaudeResult {
                    success: true,
                    stdout: "[dry_run]".to_string(),
                    stderr: String::new(),
                    duration: std::time::Duration::ZERO,
                });
            }
            ExecMode::Normal => {}
        }

        // 2. Semaphore acquire（non_blocking: interactive 用に即失敗）
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

        // 3. コマンド構築
        let turns_str = self.max_turns.to_string();
        let mut args = vec!["-p", &self.prompt];

        if let Some(ref sp) = self.system_prompt {
            args.extend(["--system-prompt", sp]);
        }
        args.extend(["--max-turns", &turns_str]);

        if let Some(ref tools) = self.allowed_tools {
            args.extend(["--allowedTools", tools]);
        }

        tracing::info!(
            "ClaudeRunner [{}]: max_turns={}, system_prompt={}, cwd={:?}, timeout={:?}s",
            self.module,
            self.max_turns,
            self.system_prompt.is_some(),
            self.cwd.as_ref().map(|p| p.display().to_string()),
            self.timeout_secs,
        );

        let mut cmd = Command::new("claude");
        cmd.args(&args);
        if let Some(ref dir) = self.cwd {
            cmd.current_dir(dir);
        }

        // 4. env_clear + 事前解決済み環境変数の注入
        if let Some(ref env_pairs) = self.resolved_env {
            cmd.env_clear();
            for (key, val) in env_pairs {
                cmd.env(key, val);
            }
        }

        // 5. タイムアウト付き実行
        let start = std::time::Instant::now();
        let output = if let Some(timeout_secs) = self.timeout_secs {
            let timeout_dur = std::time::Duration::from_secs(timeout_secs);
            // kill_on_drop(true): cmd.output() が返す future は内部で Child を所有する。
            // タイムアウトで future が drop されると Child も drop され、SIGKILL が送信される。
            // ref: https://docs.rs/tokio/latest/tokio/process/struct.Command.html#method.kill_on_drop
            cmd.kill_on_drop(true);
            let output_fut = cmd.output();
            match tokio::time::timeout(timeout_dur, output_fut).await {
                Ok(result) => result.context("Failed to execute claude -p")?,
                Err(_) => {
                    tracing::warn!(
                        "ClaudeRunner [{}]: timed out after {}s",
                        self.module,
                        timeout_secs
                    );
                    let duration = start.elapsed();
                    return Ok(ClaudeResult {
                        success: false,
                        stdout: String::new(),
                        stderr: format!("Process timed out after {}s", timeout_secs),
                        duration,
                    });
                }
            }
        } else {
            cmd.output()
                .await
                .context("Failed to execute claude -p")?
        };
        let duration = start.elapsed();

        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let success = output.status.success();

        // 6. 出力サイズ切り詰め
        let truncated = if let Some(max_bytes) = self.max_output_bytes {
            if stdout.len() > max_bytes {
                let total = stdout.len();
                let safe_end = truncate_str(&stdout, max_bytes).len();
                stdout.truncate(safe_end);
                stdout.push_str(&format!("\n[truncated, {} total bytes]", total));
                true
            } else {
                false
            }
        } else {
            false
        };

        let result = ClaudeResult {
            success,
            stdout,
            stderr,
            duration,
        };

        if !success {
            tracing::warn!(
                "ClaudeRunner [{}]: failed (exit {}): {}",
                self.module,
                output.status,
                result.stderr
            );
        }

        // 8. Hook: after_run（実行記録をレジストリに push）
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

        // 非同期ログ書き込み
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
                truncated,
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

/// 環境変数キーリストを事前解決して (key, value) ペアにする
fn resolve_env(keys: &[String]) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|key| std::env::var(key).ok().map(|val| (key.clone(), val)))
        .collect()
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

    // ローテーション
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


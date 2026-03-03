use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

use crate::repo_config::RepoEntry;

const DEFAULT_CODING_TOOLS: &str = "Read,Write,Edit,Bash,Glob,Grep";
const DEFAULT_GENERAL_TOOLS: &str = "Read,Bash,Glob,Grep,WebFetch,WebSearch";

/// soul.md が無い場合のフォールバック
const FALLBACK_CODING_SOUL: &str = "\
あなたは自律コーディングエージェントです。
承認済みのプランに従って実装を行います。";

const FALLBACK_GENERAL_SOUL: &str = "\
あなたは自律タスク実行エージェントです。
タスクの内容に基づいて作業を実行します。";

/// モード固有のルール（常に付与）
const CODING_RULES: &str = "\
## ルール
- CLAUDE.md があれば必ず読み、プロジェクトの規約に従うこと
- 既存のコードパターンや命名規則を尊重すること
- プランに記載された変更のみを行うこと（スコープ外の変更は禁止）
- テストがある場合は実行して通ることを確認すること
- 完了時に変更の要約を出力すること";

const GENERAL_RULES: &str = "\
## ルール
- タスクの指示に忠実に従うこと
- 完了時に実行結果の要約を出力すること";

/// 出力フォーマット指示（常に付与）
const OUTPUT_INSTRUCTIONS: &str = "\
## 出力（必須）
- 最終行の1つ前に `SUMMARY: 作業内容の1行要約` を出力すること
- 最終行に `MEMORY: この作業で気づいたこと・学んだことの1行メモ` を出力すること（特になければ省略可）";

pub struct ExecutionResult {
    pub success: bool,
    pub output: String,
}

fn build_system_prompt(soul: &str, skill: &str, is_coding: bool) -> String {
    let fallback = if is_coding { FALLBACK_CODING_SOUL } else { FALLBACK_GENERAL_SOUL };
    let rules = if is_coding { CODING_RULES } else { GENERAL_RULES };
    super::context::build_system_prompt(soul, fallback, rules, skill, Some(OUTPUT_INSTRUCTIONS))
}

/// 承認済みタスクを claude -p で実行
pub async fn execute_task(
    task_name: &str,
    plan_text: &str,
    repo_entry: Option<&RepoEntry>,
    repo_path: Option<&Path>,
    max_turns: u32,
    soul: &str,
    skill: &str,
    context: &str,
    memory: &str,
) -> Result<ExecutionResult> {
    let (system_prompt, allowed_tools, cwd) = if let Some(path) = repo_path {
        let tools = repo_entry
            .and_then(|r| r.allowed_tools.as_ref())
            .map(|t| t.join(","))
            .unwrap_or_else(|| DEFAULT_CODING_TOOLS.to_string());
        (build_system_prompt(soul, skill, true), tools, Some(path))
    } else {
        (
            build_system_prompt(soul, skill, false),
            DEFAULT_GENERAL_TOOLS.to_string(),
            None,
        )
    };

    let mut prompt_parts = vec![format!("## タスク\n{}\n\n## 承認済みプラン\n{}", task_name, plan_text)];

    if !context.is_empty() {
        prompt_parts.push(format!("## 直近の作業履歴\n{}", context));
    }
    if !memory.is_empty() {
        prompt_parts.push(format!("## 過去の学び・メモ\n{}", memory));
    }

    let prompt = prompt_parts.join("\n\n");

    let turns = repo_entry
        .and_then(|r| r.max_execute_turns)
        .unwrap_or(max_turns);

    tracing::info!(
        "Executing task: {} (max_turns={}, tools={}, cwd={:?})",
        task_name,
        turns,
        allowed_tools,
        cwd.map(|p| p.display().to_string()),
    );

    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        &prompt,
        "--system-prompt",
        &system_prompt,
        "--max-turns",
        &turns.to_string(),
        "--allowedTools",
        &allowed_tools,
    ]);

    if let Some(path) = cwd {
        cmd.current_dir(path);
    }

    let output = cmd
        .output()
        .await
        .context("Failed to execute claude -p")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(ExecutionResult {
            success: true,
            output: stdout,
        })
    } else {
        tracing::warn!(
            "claude -p exited with {}: {}",
            output.status,
            stderr
        );
        Ok(ExecutionResult {
            success: false,
            output: if stderr.is_empty() { stdout } else { format!("{}\n\nSTDERR:\n{}", stdout, stderr) },
        })
    }
}

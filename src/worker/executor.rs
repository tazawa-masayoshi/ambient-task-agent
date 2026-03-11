use anyhow::Result;
use std::path::Path;

use crate::claude::ClaudeRunner;
use crate::execution::RunnerContext;
use crate::repo_config::RepoEntry;
use super::context::WorkContext;

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
    /// claude -p セッションID（セッション継続に使用）
    pub session_id: Option<String>,
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
    wc: &WorkContext,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
) -> Result<ExecutionResult> {
    execute_task_with_session(task_name, plan_text, repo_entry, repo_path, wc, log_dir, runner_ctx, None).await
}

/// セッション継続対応の実行関数
#[allow(clippy::too_many_arguments)]
pub async fn execute_task_with_session(
    task_name: &str,
    plan_text: &str,
    repo_entry: Option<&RepoEntry>,
    repo_path: Option<&Path>,
    wc: &WorkContext,
    log_dir: Option<&Path>,
    runner_ctx: &RunnerContext,
    resume_session_id: Option<&str>,
) -> Result<ExecutionResult> {
    let (system_prompt, allowed_tools, cwd) = if let Some(path) = repo_path {
        let tools = repo_entry
            .and_then(|r| r.allowed_tools.as_ref())
            .map(|t| t.join(","))
            .unwrap_or_else(|| DEFAULT_CODING_TOOLS.to_string());
        (build_system_prompt(&wc.soul, &wc.skill, true), tools, Some(path))
    } else {
        (
            build_system_prompt(&wc.soul, &wc.skill, false),
            DEFAULT_GENERAL_TOOLS.to_string(),
            None,
        )
    };

    let mut prompt_parts = vec![format!("## タスク\n{}\n\n## 承認済みプラン\n{}", task_name, plan_text)];

    if !wc.context.is_empty() {
        prompt_parts.push(format!("## 直近の作業履歴\n{}", wc.context));
    }
    if !wc.memory.is_empty() {
        prompt_parts.push(format!("## 過去の学び・メモ\n{}", wc.memory));
    }

    let prompt = prompt_parts.join("\n\n");

    let turns = repo_entry
        .and_then(|r| r.max_execute_turns)
        .unwrap_or(wc.max_turns);

    let mut runner = ClaudeRunner::new("executor", &prompt)
        .system_prompt(&system_prompt)
        .max_turns(turns)
        .allowed_tools(&allowed_tools)
        .optional_log_dir(log_dir)
        .with_context(runner_ctx);

    if let Some(path) = cwd {
        runner = runner.cwd(path);
    }

    // セッション継続
    if let Some(sid) = resume_session_id {
        runner = runner.resume(sid);
    }

    let result = runner.run().await?;

    if result.success {
        Ok(ExecutionResult {
            success: true,
            output: result.stdout,
            session_id: result.session_id,
        })
    } else {
        Ok(ExecutionResult {
            success: false,
            output: if result.stderr.is_empty() {
                result.stdout
            } else {
                format!("{}\n\nSTDERR:\n{}", result.stdout, result.stderr)
            },
            session_id: result.session_id,
        })
    }
}

//! タスク分類（new → executing / conversing）
//!
//! heuristics 版と LLM 版の二段階分類。
//! few-shot 履歴が閾値未満の場合は LLM を呼ばず heuristics のみ使用する。

use std::path::Path;

use crate::db::CodingTask;
use crate::repo_config::ReposConfig;

pub enum TaskClassification {
    Execute,
    Converse,
}

/// new タスクを executing（即実行）か conversing（要件ヒアリング）に分類（heuristics 版）
pub fn classify_new_task_heuristic(task: &CodingTask, repos_config: &ReposConfig) -> TaskClassification {
    let is_slack_origin = task.asana_task_gid.starts_with("slack_")
        || task.asana_task_gid.starts_with("ops_");

    let has_repo = task.repo_key.is_some();

    let auto_execute = task.repo_key.as_deref()
        .and_then(|key| repos_config.find_repo_by_key(key))
        .map(|r| r.auto_execute)
        .unwrap_or(false);

    if is_slack_origin {
        // Slack/ops 入口: analysis_text に要件定義が入っていれば即実行
        if task.analysis_text.is_some() {
            TaskClassification::Execute
        } else {
            TaskClassification::Converse
        }
    } else {
        // Asana 入口: auto_execute フラグを尊重
        if auto_execute && has_repo {
            TaskClassification::Execute
        } else {
            TaskClassification::Converse
        }
    }
}

/// new タスクを LLM で分類（past examples を few-shot で渡す）
/// few-shot 履歴が `defaults.min_fewshot_examples` 未満の場合は heuristics にフォールバック。
/// LLM 呼び出しに失敗した場合も heuristics にフォールバック。
pub async fn classify_new_task_llm(
    task: &CodingTask,
    repos_config: &ReposConfig,
    db: &crate::db::Db,
    runner_ctx: &crate::execution::RunnerContext,
    log_dir: &Path,
) -> TaskClassification {
    // 過去の分類履歴を取得（few-shot 用）
    let history = db.get_recent_classification_history(10).unwrap_or_default();

    // 履歴が不十分な場合は LLM を呼ばず heuristics で判定（誤分類リスク回避）
    let min_examples = repos_config.defaults.min_fewshot_examples;
    if history.len() < min_examples {
        tracing::info!(
            "LLM classify skipped: only {} examples (need {}), using heuristics",
            history.len(), min_examples
        );
        return classify_new_task_heuristic(task, repos_config);
    }

    // min_fewshot_examples ガード通過後なので history は必ず非空
    let lines: Vec<String> = history.iter().map(|r| {
        format!("- 「{}」({}文字) → {} → 結果: {}", r.task_name, r.description.len(), r.classification, r.outcome)
    }).collect();
    let examples = format!("## 過去の分類履歴\n{}\n\n", lines.join("\n"));

    let desc = task.description.as_deref().unwrap_or("(なし)");
    let prompt = format!(
        "タスクを execute（即実行可能）か converse（要件確認が必要）に分類してください。\n\n\
         {}## 新しいタスク\n名前: {}\n説明: {}\nリポジトリ: {}\n入口: {}\n\n\
         判断基準:\n- 要件が明確で実装可能 → execute\n- 曖昧・不足・確認事項あり → converse",
        examples,
        task.asana_task_name,
        crate::claude::truncate_str(desc, 500),
        task.repo_key.as_deref().unwrap_or("なし"),
        task.source,
    );

    let schema = r#"{"type":"object","properties":{"classification":{"type":"string","enum":["execute","converse"]},"reason":{"type":"string"}},"required":["classification"]}"#;

    let result = crate::claude::ClaudeRunner::new("classify", &prompt)
        .max_turns(1)
        .allowed_tools("")
        .json_schema(schema)
        .log_dir(log_dir)
        .with_context(runner_ctx)
        .run()
        .await;

    match result {
        Ok(r) if r.success => {
            let answer: Option<String> = serde_json::from_str::<serde_json::Value>(&r.stdout)
                .ok()
                .and_then(|v| v.get("classification")?.as_str().map(|s| s.to_string()));

            match answer.as_deref() {
                Some("execute") => {
                    tracing::info!("LLM classify task {}: execute", task.id);
                    TaskClassification::Execute
                }
                Some("converse") => {
                    tracing::info!("LLM classify task {}: converse", task.id);
                    TaskClassification::Converse
                }
                _ => {
                    tracing::warn!("LLM classify: unexpected answer '{}', falling back to heuristics", r.stdout);
                    classify_new_task_heuristic(task, repos_config)
                }
            }
        }
        Ok(r) => {
            tracing::warn!("LLM classify failed (non-success): {}, falling back", r.stderr);
            classify_new_task_heuristic(task, repos_config)
        }
        Err(e) => {
            tracing::warn!("LLM classify error: {}, falling back to heuristics", e);
            classify_new_task_heuristic(task, repos_config)
        }
    }
}

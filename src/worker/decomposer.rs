use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtask {
    pub index: u32,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub estimated_minutes: Option<u32>,
    #[serde(default = "default_subtask_status")]
    pub status: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub actual_minutes: Option<u32>,
}

fn default_subtask_status() -> String {
    "pending".to_string()
}

/// soul.md が無い場合のフォールバック
const FALLBACK_SOUL: &str = "\
あなたは自律コーディングエージェントのタスク分解スペシャリストです。
要件定義をもとに、実装可能なサブタスクに分解します。";

/// 分解ルール
const DECOMPOSER_RULES: &str = r#"## ルール
- 要件定義の内容を正確に反映すること
- 各サブタスクは独立して実装可能な粒度にすること
- ファイルパスを具体的に含めること
- コードベースを読んで実装の現実性を確認すること
- ファイルの変更は一切行わないこと（読み取り専用）
- サブタスク間の依存関係を depends_on で明示すること（先行タスクの index を指定）
- 各サブタスクの作業時間を estimated_minutes で見積もること（分単位）

## 出力フォーマット（厳守）
以下の JSON 配列のみを出力してください。説明文やコードブロックは不要です:

[
  {"index": 1, "title": "サブタスクのタイトル", "detail": "具体的な作業内容（ファイルパス含む）", "depends_on": [], "estimated_minutes": 30},
  {"index": 2, "title": "...", "detail": "...", "depends_on": [1], "estimated_minutes": 45}
]"#;

fn build_system_prompt(soul: &str, skill: &str) -> String {
    super::context::build_system_prompt(soul, FALLBACK_SOUL, DECOMPOSER_RULES, skill, None)
}

/// claude -p でタスクを分解
pub async fn decompose_task(
    task_name: &str,
    analysis: &str,
    repo_path: &Path,
    max_turns: u32,
    soul: &str,
    skill: &str,
    context: &str,
    memory: &str,
) -> Result<Vec<Subtask>> {
    let system_prompt = build_system_prompt(soul, skill);

    let mut prompt_parts = vec![
        format!("## タスク\n{}", task_name),
        format!("## 要件定義\n{}", analysis),
    ];

    if !context.is_empty() {
        prompt_parts.push(format!("## 直近の作業履歴\n{}", context));
    }
    if !memory.is_empty() {
        prompt_parts.push(format!("## 過去の学び・メモ\n{}", memory));
    }

    let prompt = prompt_parts.join("\n\n");

    tracing::info!(
        "Running decomposer claude -p in {} (max_turns={})",
        repo_path.display(),
        max_turns
    );

    let output = Command::new("claude")
        .args([
            "-p",
            &prompt,
            "--system-prompt",
            &system_prompt,
            "--max-turns",
            &max_turns.to_string(),
        ])
        .current_dir(repo_path)
        .output()
        .await
        .context("Failed to execute claude -p for decomposition")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "claude -p decomposition failed (exit {}): {}",
            output.status,
            stderr
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    parse_subtasks(&raw)
}

/// JSON 配列をパース（コードブロックで囲まれている場合も対応）
fn parse_subtasks(raw: &str) -> Result<Vec<Subtask>> {
    let trimmed = raw.trim();

    // ```json ... ``` で囲まれている場合を除去
    let json_str = if trimmed.starts_with("```") {
        match (trimmed.find('['), trimmed.rfind(']')) {
            (Some(s), Some(e)) => &trimmed[s..e + 1],
            _ => anyhow::bail!("Could not find JSON array in code block"),
        }
    } else if trimmed.starts_with('[') {
        trimmed
    } else {
        // JSON 配列部分を検索
        let start = trimmed.find('[');
        let end = trimmed.rfind(']').map(|i| i + 1);
        match (start, end) {
            (Some(s), Some(e)) => &trimmed[s..e],
            _ => anyhow::bail!("Could not find JSON array in decomposer output"),
        }
    };

    let subtasks: Vec<Subtask> =
        serde_json::from_str(json_str).context("Failed to parse subtasks JSON")?;
    Ok(subtasks)
}

/// サブタスクの進捗率を計算 (0-100)
pub fn calculate_progress(subtasks: &[Subtask]) -> i32 {
    if subtasks.is_empty() {
        return 0;
    }
    let done = subtasks.iter().filter(|s| s.status == "done").count();
    ((done as f64 / subtasks.len() as f64) * 100.0).round() as i32
}

/// 依存先が未完了のサブタスクを blocked に設定
pub fn detect_blocked_subtasks(subtasks: &mut [Subtask]) {
    let done_indices: std::collections::HashSet<u32> = subtasks
        .iter()
        .filter(|s| s.status == "done")
        .map(|s| s.index)
        .collect();

    for s in subtasks.iter_mut() {
        if s.status == "done" || s.status == "in_progress" {
            continue;
        }
        if !s.depends_on.is_empty() && !s.depends_on.iter().all(|dep| done_indices.contains(dep)) {
            s.status = "blocked".to_string();
        } else if s.status == "blocked" {
            // 依存が解決されたら pending に戻す
            s.status = "pending".to_string();
        }
    }
}

/// 着手可能なサブタスク（pending + 依存解決済み）を返す
pub fn get_actionable_subtasks(subtasks: &[Subtask]) -> Vec<&Subtask> {
    let done_indices: std::collections::HashSet<u32> = subtasks
        .iter()
        .filter(|s| s.status == "done")
        .map(|s| s.index)
        .collect();

    subtasks
        .iter()
        .filter(|s| {
            s.status == "pending"
                && (s.depends_on.is_empty()
                    || s.depends_on.iter().all(|dep| done_indices.contains(dep)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_subtasks_plain() {
        let input = r#"[
            {"index": 1, "title": "バリデーション関数", "detail": "src/validators.rs に作成"},
            {"index": 2, "title": "フォーム統合", "detail": "src/pages/Login.tsx に組み込み"}
        ]"#;
        let result = parse_subtasks(input).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].index, 1);
        assert_eq!(result[1].title, "フォーム統合");
        // 後方互換: 新フィールドはデフォルト値
        assert!(result[0].depends_on.is_empty());
        assert_eq!(result[0].status, "pending");
        assert!(result[0].estimated_minutes.is_none());
    }

    #[test]
    fn test_parse_subtasks_codeblock() {
        let input = "Here are the subtasks:\n```json\n[\n{\"index\": 1, \"title\": \"Task 1\", \"detail\": \"Details\"}\n]\n```\nDone.";
        let result = parse_subtasks(input).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_parse_subtasks_with_prefix() {
        let input = "The decomposed subtasks:\n\n[{\"index\": 1, \"title\": \"First\", \"detail\": \"Do this\"}]";
        let result = parse_subtasks(input).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_parse_subtasks_with_new_fields() {
        let input = r#"[
            {"index": 1, "title": "DB migration", "detail": "Add columns", "depends_on": [], "estimated_minutes": 30},
            {"index": 2, "title": "API endpoint", "detail": "Add route", "depends_on": [1], "estimated_minutes": 45}
        ]"#;
        let result = parse_subtasks(input).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].depends_on.is_empty());
        assert_eq!(result[0].estimated_minutes, Some(30));
        assert_eq!(result[1].depends_on, vec![1]);
        assert_eq!(result[1].estimated_minutes, Some(45));
        assert_eq!(result[1].status, "pending");
    }

    #[test]
    fn test_calculate_progress() {
        let subtasks = vec![
            Subtask { index: 1, title: "A".into(), detail: "".into(), depends_on: vec![], estimated_minutes: None, status: "done".into(), started_at: None, completed_at: None, actual_minutes: None },
            Subtask { index: 2, title: "B".into(), detail: "".into(), depends_on: vec![], estimated_minutes: None, status: "in_progress".into(), started_at: None, completed_at: None, actual_minutes: None },
            Subtask { index: 3, title: "C".into(), detail: "".into(), depends_on: vec![], estimated_minutes: None, status: "pending".into(), started_at: None, completed_at: None, actual_minutes: None },
        ];
        assert_eq!(calculate_progress(&subtasks), 33);
        assert_eq!(calculate_progress(&[]), 0);
    }

    #[test]
    fn test_detect_blocked() {
        let mut subtasks = vec![
            Subtask { index: 1, title: "A".into(), detail: "".into(), depends_on: vec![], estimated_minutes: None, status: "pending".into(), started_at: None, completed_at: None, actual_minutes: None },
            Subtask { index: 2, title: "B".into(), detail: "".into(), depends_on: vec![1], estimated_minutes: None, status: "pending".into(), started_at: None, completed_at: None, actual_minutes: None },
        ];
        detect_blocked_subtasks(&mut subtasks);
        assert_eq!(subtasks[0].status, "pending");
        assert_eq!(subtasks[1].status, "blocked");

        // #1 を done にすると #2 が unblock
        subtasks[0].status = "done".into();
        detect_blocked_subtasks(&mut subtasks);
        assert_eq!(subtasks[1].status, "pending");
    }

    #[test]
    fn test_get_actionable() {
        let subtasks = vec![
            Subtask { index: 1, title: "A".into(), detail: "".into(), depends_on: vec![], estimated_minutes: None, status: "done".into(), started_at: None, completed_at: None, actual_minutes: None },
            Subtask { index: 2, title: "B".into(), detail: "".into(), depends_on: vec![1], estimated_minutes: None, status: "pending".into(), started_at: None, completed_at: None, actual_minutes: None },
            Subtask { index: 3, title: "C".into(), detail: "".into(), depends_on: vec![2], estimated_minutes: None, status: "pending".into(), started_at: None, completed_at: None, actual_minutes: None },
        ];
        let actionable = get_actionable_subtasks(&subtasks);
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].index, 2);
    }
}
